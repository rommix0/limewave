//! Real-time audio output: a fixed voice mixer fed by lock-free command queues.
//!
//! The audio callback never allocates, locks, blocks, prints or panics. The UI publishes a new
//! [`PlayBuffer`] through an [`ArcSwap`] and sends [`Command`]s through `rtrb` ring buffers.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, I24, SampleFormat, SizedSample, StreamConfig, U24};
use limewave_core::edit::LoopMode;

use super::voice::{Adsr, MAX_VOICES, PlayBuffer, Voice};

/// Capacity of each command ring buffer.
const COMMAND_CAPACITY: usize = 256;
/// Frames of the conversion scratch buffer used for non-`f32` devices.
const SCRATCH_FRAMES: usize = 16_384;

/// A message from the UI or MIDI thread to the audio callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    NoteOn {
        note: u8,
        from: usize,
    },
    NoteOff {
        note: u8,
    },
    AllOff,
    /// Replaces the envelope for new voices and the release time of sounding ones.
    SetEnvelope(Adsr),
    /// Plays `from` until the voice reaches `to`, ignoring the loop and End, then releases.
    PlayRange {
        note: u8,
        from: usize,
        to: usize,
    },
    /// Whether voices started from now on leave the loop when released and play on to End
    /// (v0.3 §7.3). Sounding voices keep the setting they started with.
    SetReleasePastLoop(bool),
    /// A dialog preview (plan ruling R2): like `NoteOn`, with the session attack only (no
    /// decay, full sustain), but its
    /// release always leaves the loop and holds its level until End, so the generated release
    /// is heard as it is. `NoteOff` releases it that way; `AllOff` (Stop) fades it out over
    /// [`STEAL_FADE_MS`], so Stop silences it at once, as does a new note on the same key.
    /// With `hold_loop` false the voice ignores the loop from the start and plays straight
    /// through to End (a Decay tail, v0.3 §7.1); with it true it holds on the loop until
    /// released (a Sustain tail, a spectrum loop).
    PreviewOn {
        note: u8,
        from: usize,
        hold_loop: bool,
    },
}

/// Where one sounding voice is: its read position in file frames and its envelope level.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cursor {
    pub frame: f64,
    pub level: f32,
}

/// The voices' positions, published by the audio thread for the UI to draw as cursors.
///
/// One slot per voice: the `f64` bits of the frame and the `f32` bits of the level, where a
/// level of 0 marks an empty slot. The audio thread writes every slot once per callback with
/// plain atomic stores (no allocation, no locks). The frame is stored `Relaxed` and the level
/// with `Release`; an empty slot stores only its level, keeping its last frame.
/// [`Cursors::read`] loads the level with `Acquire` before the frame, so a cursor never pairs
/// a new level with an older frame. Slots are independent: a read may mix slots from two
/// consecutive callbacks, which at most moves one cursor by a few milliseconds.
pub struct Cursors {
    frames: [AtomicU64; MAX_VOICES],
    levels: [AtomicU32; MAX_VOICES],
}

impl Cursors {
    pub fn new() -> Cursors {
        Cursors {
            frames: [const { AtomicU64::new(0) }; MAX_VOICES],
            levels: [const { AtomicU32::new(0) }; MAX_VOICES],
        }
    }

    /// The cursors of the voices currently sounding. UI side; allocates.
    pub fn read(&self) -> Vec<Cursor> {
        self.levels
            .iter()
            .zip(&self.frames)
            .filter_map(|(level, frame)| {
                let level = f32::from_bits(level.load(Ordering::Acquire));
                (level > 0.0).then(|| Cursor {
                    frame: f64::from_bits(frame.load(Ordering::Relaxed)),
                    level,
                })
            })
            .collect()
    }

    /// Writes every slot from `voices`. Audio side; does not allocate.
    fn publish(&self, voices: &[Option<Voice>; MAX_VOICES]) {
        for (i, voice) in voices.iter().enumerate() {
            // An empty slot only clears its level: a reader that loaded the old level must
            // not then load a frame of 0 and draw a cursor there.
            let level = voice.as_ref().map_or(0.0, |v| {
                self.frames[i].store(v.position().to_bits(), Ordering::Relaxed);
                v.level()
            });
            self.levels[i].store(level.to_bits(), Ordering::Release);
        }
    }
}

impl Default for Cursors {
    fn default() -> Self {
        Self::new()
    }
}

/// How long a stolen voice takes to fade out.
const STEAL_FADE_MS: f32 = 5.0;
/// Spare slots where stolen voices fade out.
const DYING_VOICES: usize = 4;

/// Voice slots owned by the audio callback.
pub struct MixerState {
    voices: [Option<Voice>; MAX_VOICES],
    /// Stolen voices finishing their short fade. They are not cursors and take no commands.
    dying: [Option<Voice>; DYING_VOICES],
    age: [u64; MAX_VOICES],
    counter: u64,
    /// The envelope new voices start with.
    adsr: Adsr,
    /// Whether new voices leave the loop on release ([`Command::SetReleasePastLoop`]).
    release_past_loop: bool,
}

impl MixerState {
    pub fn new() -> MixerState {
        MixerState {
            voices: [const { None }; MAX_VOICES],
            dying: [const { None }; DYING_VOICES],
            age: [0; MAX_VOICES],
            counter: 0,
            adsr: Adsr::DEFAULT,
            release_past_loop: false,
        }
    }

    fn apply(&mut self, cmd: Command, buf: &PlayBuffer, device_rate: u32) {
        match cmd {
            Command::NoteOn { note, from } => {
                self.note_on(note, from, None, None, buf, device_rate);
            }
            Command::PreviewOn {
                note,
                from,
                hold_loop,
            } => {
                self.note_on(note, from, None, Some(hold_loop), buf, device_rate);
            }
            Command::SetReleasePastLoop(on) => self.release_past_loop = on,
            Command::PlayRange { note, from, to } => {
                self.note_on(note, from, Some(to), None, buf, device_rate);
            }
            Command::NoteOff { note } => self
                .voices
                .iter_mut()
                .flatten()
                .filter(|v| v.note() == note)
                .for_each(Voice::release),
            Command::AllOff => {
                for v in self.voices.iter_mut().flatten() {
                    if v.is_preview() {
                        v.fade_out(STEAL_FADE_MS);
                    } else {
                        v.release();
                    }
                }
            }
            Command::SetEnvelope(a) => {
                let a = a.clamped();
                self.adsr = a;
                for v in self.voices.iter_mut().flatten() {
                    v.set_release_ms(a.release_ms);
                }
            }
        }
    }

    /// Releases any voice already playing `note` (a preview fades out), then starts a new one in a free slot. With no
    /// free slot a voice is stolen: the one that was playing `note`, else the oldest releasing
    /// voice, else the oldest. The stolen voice fades out over [`STEAL_FADE_MS`] in a spare
    /// slot rather than being cut. `preview` (`Some(hold_loop)`) starts a
    /// [`Command::PreviewOn`] voice.
    fn note_on(
        &mut self,
        note: u8,
        from: usize,
        stop_at: Option<usize>,
        preview: Option<bool>,
        buf: &PlayBuffer,
        device_rate: u32,
    ) {
        let mut existing = None;
        for (i, slot) in self.voices.iter_mut().enumerate() {
            if let Some(v) = slot.as_mut().filter(|v| v.note() == note) {
                // A preview released here would play its whole held release under the new
                // note, so it fades out instead.
                if v.is_preview() {
                    v.fade_out(STEAL_FADE_MS);
                } else {
                    v.release();
                }
                existing = Some(i);
            }
        }
        let slot = match self.voices.iter().position(Option::is_none) {
            Some(free) => free,
            None => {
                let victim = existing.unwrap_or_else(|| self.victim());
                self.retire(victim);
                victim
            }
        };
        let voice = Voice::new(note, from, buf, device_rate, self.adsr, stop_at)
            .release_past_loop(self.release_past_loop);
        self.voices[slot] = Some(match preview {
            Some(hold_loop) => voice.preview(hold_loop),
            None => voice,
        });
        self.age[slot] = self.counter;
        self.counter += 1;
    }

    /// The slot to steal: the oldest releasing voice, else the oldest voice.
    fn victim(&self) -> usize {
        let held = |i: usize| !self.voices[i].as_ref().is_some_and(Voice::is_releasing);
        (0..MAX_VOICES)
            .min_by_key(|&i| (held(i), self.age[i]))
            .unwrap_or(0)
    }

    /// Moves the voice in `slot` to a spare slot to fade out. With every spare slot busy, the
    /// quietest dying voice there makes way.
    fn retire(&mut self, slot: usize) {
        let Some(mut v) = self.voices[slot].take() else {
            return;
        };
        v.fade_out(STEAL_FADE_MS);
        let spare = self
            .dying
            .iter()
            .position(Option::is_none)
            .or_else(|| {
                (0..DYING_VOICES).min_by(|&a, &b| {
                    let level = |i: usize| self.dying[i].as_ref().map_or(0.0, Voice::level);
                    level(a).total_cmp(&level(b))
                })
            })
            .unwrap_or(0);
        self.dying[spare] = Some(v);
    }
}

impl Default for MixerState {
    fn default() -> Self {
        Self::new()
    }
}

/// The body of the audio callback: applies pending commands (new notes play `current`), then
/// fills interleaved `out` (cleared first) with the mixed voices, stolen ones still fading
/// included, clamped to [-1, 1]. Each
/// voice plays whichever of `current` and `previous` has its source; a voice whose source is
/// in neither is dropped. Afterwards the voices' positions are published to `cursors`. Does not
/// allocate.
#[allow(clippy::too_many_arguments)]
pub fn process(
    state: &mut MixerState,
    current: &PlayBuffer,
    previous: &PlayBuffer,
    cmds: &mut rtrb::Consumer<Command>,
    cursors: &Cursors,
    out: &mut [f32],
    channels: usize,
    device_rate: u32,
) {
    while let Ok(cmd) = cmds.pop() {
        state.apply(cmd, current, device_rate);
    }
    out.fill(0.0);
    if channels > 0 {
        for slot in state.voices.iter_mut().chain(&mut state.dying) {
            let Some(v) = slot else { continue };
            let buf = [current, previous]
                .into_iter()
                .find(|b| b.source == v.source());
            if !buf.is_some_and(|b| v.render_add(b, out, channels, device_rate)) {
                *slot = None;
            }
        }
        for s in out {
            *s = s.clamp(-1.0, 1.0);
        }
    }
    cursors.publish(&state.voices);
}

/// Everything the audio callback owns.
struct Callback {
    state: MixerState,
    buffer: Arc<ArcSwap<PlayBuffer>>,
    /// The buffer last loaded, and the last one from a different, non-empty source before it,
    /// which voices released on the previous file finish on.
    ///
    /// The callback never frees a buffer: everything it holds came from `buffer`, and
    /// `AudioEngine::set_buffer` keeps each buffer it swaps out in its `retired` list until
    /// only that list holds it. So dropping one of these here is never the last reference.
    current: Arc<PlayBuffer>,
    previous: Arc<PlayBuffer>,
    ui_rx: rtrb::Consumer<Command>,
    midi_rx: rtrb::Consumer<Command>,
    cursors: Arc<Cursors>,
    channels: usize,
    device_rate: u32,
}

impl Callback {
    fn new(
        buffer: Arc<ArcSwap<PlayBuffer>>,
        cursors: Arc<Cursors>,
        ui_rx: rtrb::Consumer<Command>,
        midi_rx: rtrb::Consumer<Command>,
        channels: usize,
        device_rate: u32,
    ) -> Callback {
        let current = buffer.load_full();
        Callback {
            state: MixerState::new(),
            previous: Arc::clone(&current),
            current,
            buffer,
            ui_rx,
            midi_rx,
            cursors,
            channels,
            device_rate,
        }
    }

    fn render(&mut self, out: &mut [f32]) {
        // `load_full` only bumps a reference count. A buffer from another source pushes the
        // old one to `previous`; the same source replaces it, so live edits are heard at once.
        // The empty buffer (source 0) never becomes `previous`: after a switch through it (a
        // file still loading), voices released on the file before it can still finish.
        let loaded = self.buffer.load_full();
        if loaded.source == self.current.source {
            self.current = loaded;
        } else {
            let old = std::mem::replace(&mut self.current, loaded);
            if old.source != 0 {
                self.previous = old;
            }
        }
        while let Ok(cmd) = self.midi_rx.pop() {
            self.state.apply(cmd, &self.current, self.device_rate);
        }
        process(
            &mut self.state,
            &self.current,
            &self.previous,
            &mut self.ui_rx,
            &self.cursors,
            out,
            self.channels,
            self.device_rate,
        );
    }
}

/// The running output stream plus the UI-side ends of its queues.
pub struct AudioEngine {
    pub device_rate: u32,
    pub device_name: String,
    // Declared before the buffers so the stream (and its callback) is dropped first. `None`
    // only in tests, where the callback is driven by hand.
    _stream: Option<cpal::Stream>,
    buffer: Arc<ArcSwap<PlayBuffer>>,
    /// Buffers swapped out by `set_buffer` that the audio thread may still hold. Each is dropped
    /// here on the UI thread once nothing else holds it, so the audio thread never frees one.
    retired: RefCell<Vec<Arc<PlayBuffer>>>,
    cursors: Arc<Cursors>,
    ui_tx: rtrb::Producer<Command>,
    midi_tx: Option<rtrb::Producer<Command>>,
}

impl AudioEngine {
    /// Opens the default output device with its default config and starts playing silence.
    pub fn start() -> Result<AudioEngine, String> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| "No audio output device found".to_string())?;
        let device_name = device
            .description()
            .map_or_else(|_| "Unknown device".to_string(), |d| d.name().to_string());
        let supported = device
            .default_output_config()
            .map_err(|e| format!("No usable output config on {device_name}: {e}"))?;
        let format = supported.sample_format();
        let config = supported.config();
        let device_rate = config.sample_rate;

        let buffer = Arc::new(ArcSwap::from_pointee(empty_buffer(device_rate)));
        let (ui_tx, ui_rx) = rtrb::RingBuffer::new(COMMAND_CAPACITY);
        let (midi_tx, midi_rx) = rtrb::RingBuffer::new(COMMAND_CAPACITY);
        let cursors = Arc::new(Cursors::new());
        let cb = Callback::new(
            Arc::clone(&buffer),
            Arc::clone(&cursors),
            ui_rx,
            midi_rx,
            usize::from(config.channels),
            device_rate,
        );
        let stream = match format {
            SampleFormat::F32 => build_f32(&device, config, cb),
            SampleFormat::F64 => build_converted::<f64>(&device, config, cb),
            SampleFormat::I8 => build_converted::<i8>(&device, config, cb),
            SampleFormat::I16 => build_converted::<i16>(&device, config, cb),
            SampleFormat::I24 => build_converted::<I24>(&device, config, cb),
            SampleFormat::I32 => build_converted::<i32>(&device, config, cb),
            SampleFormat::I64 => build_converted::<i64>(&device, config, cb),
            SampleFormat::U8 => build_converted::<u8>(&device, config, cb),
            SampleFormat::U16 => build_converted::<u16>(&device, config, cb),
            SampleFormat::U24 => build_converted::<U24>(&device, config, cb),
            SampleFormat::U32 => build_converted::<u32>(&device, config, cb),
            SampleFormat::U64 => build_converted::<u64>(&device, config, cb),
            // DSD and any format added later.
            other => return Err(format!("Unsupported output sample format {other}")),
        }
        .map_err(|e| format!("Could not open audio output {device_name}: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("Could not start audio output {device_name}: {e}"))?;

        Ok(AudioEngine {
            device_rate,
            device_name,
            _stream: Some(stream),
            buffer,
            retired: RefCell::new(Vec::new()),
            cursors,
            ui_tx,
            midi_tx: Some(midi_tx),
        })
    }

    /// Publishes a new buffer for the voices to play.
    pub fn set_buffer(&self, b: PlayBuffer) {
        let old = self.buffer.swap(Arc::new(b));
        let mut retired = self.retired.borrow_mut();
        retired.push(old);
        // After `swap`, a reader still holding an old buffer owns a counted reference, so a count
        // of 1 means only this list holds it and dropping it here is the final free.
        retired.retain(|b| Arc::strong_count(b) > 1);
    }

    /// The cursors of the voices sounding as of the last audio callback.
    pub fn cursors(&self) -> Vec<Cursor> {
        self.cursors.read()
    }

    /// Queues a command for the audio thread. Dropped if the queue is full.
    pub fn send(&mut self, c: Command) {
        let _ = self.ui_tx.push(c);
    }

    /// Sets the envelope for new notes and the release of sounding ones (clamped first).
    pub fn set_envelope(&mut self, a: Adsr) {
        self.send(Command::SetEnvelope(a.clamped()));
    }

    /// Releases every note and swaps in an empty buffer, so no new note can start from the
    /// previous file. Released voices finish their envelope on that file's audio.
    pub fn silence(&mut self) {
        self.send(Command::AllOff);
        self.set_buffer(empty_buffer(self.device_rate));
    }

    /// The producer end of the MIDI command queue. `Some` on the first call only.
    pub fn midi_sender(&mut self) -> Option<rtrb::Producer<Command>> {
        self.midi_tx.take()
    }
}

/// An engine without an output device, for tests: its callback is driven by hand through the
/// returned [`HeadlessOutput`].
#[cfg(test)]
pub(crate) fn headless(device_rate: u32) -> (AudioEngine, HeadlessOutput) {
    let buffer = Arc::new(ArcSwap::from_pointee(empty_buffer(device_rate)));
    let (ui_tx, ui_rx) = rtrb::RingBuffer::new(COMMAND_CAPACITY);
    let (midi_tx, midi_rx) = rtrb::RingBuffer::new(COMMAND_CAPACITY);
    let cursors = Arc::new(Cursors::new());
    let cb = Callback::new(
        Arc::clone(&buffer),
        Arc::clone(&cursors),
        ui_rx,
        midi_rx,
        2,
        device_rate,
    );
    let engine = AudioEngine {
        device_rate,
        device_name: "headless".to_string(),
        _stream: None,
        buffer,
        retired: RefCell::new(Vec::new()),
        cursors,
        ui_tx,
        midi_tx: Some(midi_tx),
    };
    (engine, HeadlessOutput(cb))
}

/// The audio callback of a [`headless`] engine.
#[cfg(test)]
pub(crate) struct HeadlessOutput(Callback);

#[cfg(test)]
impl HeadlessOutput {
    /// Runs the callback for `frames` stereo frames and returns them interleaved.
    pub(crate) fn render(&mut self, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        self.0.render(&mut out);
        out
    }

    /// The sources of the buffers the callback holds: the current one and the previous one.
    pub(crate) fn sources(&self) -> (u64, u64) {
        (self.0.current.source, self.0.previous.source)
    }

    /// The source each sounding voice plays.
    pub(crate) fn voice_sources(&self) -> Vec<u64> {
        self.0
            .state
            .voices
            .iter()
            .flatten()
            .map(Voice::source)
            .collect()
    }
}

fn empty_buffer(rate: u32) -> PlayBuffer {
    PlayBuffer {
        audio: Vec::new(),
        rate,
        start: 0,
        end: 0,
        loop_mode: LoopMode::Off,
        loop_start: 0,
        loop_end: 0,
        root: 60,
        cents: 0.0,
        source: 0,
    }
}

fn build_f32(
    device: &cpal::Device,
    config: StreamConfig,
    mut cb: Callback,
) -> Result<cpal::Stream, cpal::Error> {
    device.build_output_stream::<f32, _, _>(
        config,
        move |data, _| cb.render(data),
        report_stream_error,
        None,
    )
}

/// Renders into a scratch buffer allocated here, before the stream starts, then converts.
/// Callbacks larger than the scratch buffer are processed in chunks.
fn build_converted<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut cb: Callback,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let chunk = SCRATCH_FRAMES * cb.channels.max(1);
    let mut scratch = vec![0.0f32; chunk];
    device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            for out in data.chunks_mut(chunk) {
                let tmp = &mut scratch[..out.len()];
                cb.render(tmp);
                for (o, s) in out.iter_mut().zip(tmp.iter()) {
                    *o = T::from_sample_(*s);
                }
            }
        },
        report_stream_error,
        None,
    )
}

/// Stream error callback. Not the real-time data path; such errors are rare and terminal.
fn report_stream_error(e: cpal::Error) {
    eprintln!("audio stream error: {e}");
}

#[cfg(test)]
mod tests {
    use super::super::voice::Adsr;
    use super::*;
    use limewave_core::edit::LoopMode;

    const RATE: u32 = 48_000;
    /// No attack or decay, so a voice plays at full level until its 10 ms (480 frame) release.
    const FLAT: Adsr = Adsr {
        attack_ms: 0.0,
        decay_ms: 0.0,
        sustain: 1.0,
        release_ms: 10.0,
    };

    /// A looping stereo buffer of constant `value`, so every voice keeps sounding.
    fn constant(source: u64, value: f32) -> PlayBuffer {
        PlayBuffer {
            audio: vec![vec![value; 4000], vec![value; 4000]],
            rate: RATE,
            start: 0,
            end: 3999,
            loop_mode: LoopMode::Forward,
            loop_start: 1000,
            loop_end: 2999,
            root: 60,
            cents: 0.0,
            source,
        }
    }

    fn looping() -> PlayBuffer {
        constant(1, 0.25)
    }

    fn active(state: &MixerState) -> Vec<u8> {
        state.voices.iter().flatten().map(Voice::note).collect()
    }

    fn block(
        state: &mut MixerState,
        b: &PlayBuffer,
        rx: &mut rtrb::Consumer<Command>,
        frames: usize,
    ) -> Vec<f32> {
        block_with(state, b, rx, &Cursors::new(), frames)
    }

    /// Like [`block`], publishing the voices' positions to `cursors`.
    fn block_with(
        state: &mut MixerState,
        b: &PlayBuffer,
        rx: &mut rtrb::Consumer<Command>,
        cursors: &Cursors,
        frames: usize,
    ) -> Vec<f32> {
        let mut out = vec![0.7; frames * 2];
        process(state, b, b, rx, cursors, &mut out, 2, RATE);
        out
    }

    /// The left channel of interleaved stereo `out`.
    fn left(out: &[f32]) -> Vec<f32> {
        out.iter().step_by(2).copied().collect()
    }

    /// A callback reading `first` from its own [`ArcSwap`], with the UI command queue.
    fn callback(
        first: PlayBuffer,
    ) -> (Callback, Arc<ArcSwap<PlayBuffer>>, rtrb::Producer<Command>) {
        let swap = Arc::new(ArcSwap::from_pointee(first));
        let (tx, rx) = rtrb::RingBuffer::new(256);
        let (_, midi_rx) = rtrb::RingBuffer::new(256);
        let cb = Callback::new(Arc::clone(&swap), Arc::default(), rx, midi_rx, 2, RATE);
        (cb, swap, tx)
    }

    fn render(cb: &mut Callback, frames: usize) -> Vec<f32> {
        let mut out = vec![0.7; frames * 2];
        cb.render(&mut out);
        out
    }

    #[test]
    fn engine_mixer_handles_commands_without_device() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();

        // Nothing playing: the stale output is cleared to silence.
        let out = block(&mut state, &b, &mut rx, 64);
        assert!(out.iter().all(|s| *s == 0.0));

        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        let out = block(&mut state, &b, &mut rx, 64);
        assert!(
            out.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "{:?}",
            &out[..4]
        );

        tx.push(Command::NoteOff { note: 60 }).unwrap();
        block(&mut state, &b, &mut rx, 480);
        let out = block(&mut state, &b, &mut rx, 480);
        assert!(out.iter().all(|s| *s == 0.0));
        assert!(active(&state).is_empty());

        // Nine notes: the oldest (60) is stolen and eight remain.
        for note in 60..69 {
            tx.push(Command::NoteOn { note, from: 0 }).unwrap();
        }
        let out = block(&mut state, &b, &mut rx, 64);
        let mut notes = active(&state);
        notes.sort_unstable();
        assert_eq!(notes, (61..69).collect::<Vec<u8>>());
        // Eight voices of 0.25 sum to 2.0; the output is clamped.
        assert!(out.iter().all(|s| *s == 1.0));
    }

    #[test]
    fn all_off_releases_through_the_envelope() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        // The default envelope: a 2 ms attack and a 30 ms (1440 frame) release.
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        tx.push(Command::NoteOn { note: 64, from: 0 }).unwrap();
        let out = block(&mut state, &b, &mut rx, 200);
        assert!(out[0] < 0.1, "the attack starts near silence");
        assert!(
            (out[2 * 199] - 0.5).abs() < 1e-6,
            "two voices at full level"
        );

        tx.push(Command::AllOff).unwrap();
        let out = left(&block(&mut state, &b, &mut rx, 1439));
        assert_eq!(active(&state).len(), 2, "still releasing");
        assert!(out[0] <= 0.5 && out[0] > 0.49, "no jump: {}", out[0]);
        assert!(out.windows(2).all(|w| w[1] < w[0]), "fades every frame");
        assert!(out[1438] < 0.01);
        let out = block(&mut state, &b, &mut rx, 1);
        assert!(out[0] < 0.001);
        assert!(active(&state).is_empty(), "released after 30 ms");
    }

    #[test]
    fn set_envelope_reaches_new_and_sounding_voices() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 500);
        let quick = Adsr {
            attack_ms: 10.0,
            decay_ms: 0.0,
            sustain: 1.0,
            release_ms: 5.0,
        };
        tx.push(Command::SetEnvelope(quick)).unwrap();
        let out = block(&mut state, &b, &mut rx, 10);
        assert!(
            out.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "held voice keeps sounding"
        );

        tx.push(Command::NoteOff { note: 60 }).unwrap();
        block(&mut state, &b, &mut rx, 240);
        assert!(active(&state).is_empty(), "the new 5 ms release applied");

        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        let out = left(&block(&mut state, &b, &mut rx, 600));
        let tol = 0.25 / 480.0 + 1e-6;
        assert!(
            (out[240] - 0.125).abs() <= tol,
            "10 ms attack: {}",
            out[240]
        );
        assert!((out[479] - 0.25).abs() <= tol, "{}", out[479]);
    }

    #[test]
    fn engine_retrigger_releases_the_sounding_note() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 64);
        // A free slot exists: the old voice fades while the new one starts.
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 1);
        assert_eq!(active(&state), [60, 60]);
        block(&mut state, &b, &mut rx, 480);
        assert_eq!(active(&state), [60]);

        // All slots busy: retriggering restarts the note in place instead of stealing.
        for note in 61..68 {
            tx.push(Command::NoteOn { note, from: 0 }).unwrap();
        }
        tx.push(Command::NoteOn { note: 61, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 1);
        let mut notes = active(&state);
        notes.sort_unstable();
        assert_eq!(notes, (60..68).collect::<Vec<u8>>());
    }

    #[test]
    fn a_stolen_voice_fades_out_instead_of_cutting() {
        // Eight voices of 0.1 stay below the output clamp, so every step shows.
        let b = constant(1, 0.1);
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        // A 5 ms (240 frame) attack, so the new note itself rises slowly too.
        let slow = Adsr {
            attack_ms: 5.0,
            decay_ms: 0.0,
            sustain: 1.0,
            release_ms: 1000.0,
        };
        tx.push(Command::SetEnvelope(slow)).unwrap();
        for note in 60..68 {
            tx.push(Command::NoteOn { note, from: 0 }).unwrap();
        }
        let mut out = left(&block(&mut state, &b, &mut rx, 400));
        tx.push(Command::NoteOn { note: 68, from: 0 }).unwrap();
        out.extend(left(&block(&mut state, &b, &mut rx, 600)));
        assert!(
            (out[399] - 0.8).abs() < 1e-5,
            "eight held voices: {}",
            out[399]
        );

        let worst = out[300..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 2.0 * 0.1 / 240.0 + 1e-6, "a step of {worst}");
        let mut notes = active(&state);
        notes.sort_unstable();
        assert_eq!(notes, (61..69).collect::<Vec<u8>>());
        assert!((out[999] - 0.8).abs() < 1e-5, "the stolen voice is gone");
    }

    #[test]
    fn a_releasing_voice_is_stolen_before_a_held_one() {
        let b = constant(1, 0.1);
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::SetEnvelope(Adsr {
            release_ms: 1000.0,
            ..FLAT
        }))
        .unwrap();
        // 60 is the oldest and still held; 67 is the newest and releasing.
        for note in 60..68 {
            tx.push(Command::NoteOn { note, from: 0 }).unwrap();
        }
        tx.push(Command::NoteOff { note: 67 }).unwrap();
        block(&mut state, &b, &mut rx, 64);
        tx.push(Command::NoteOn { note: 68, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 1);
        let mut notes = active(&state);
        notes.sort_unstable();
        assert_eq!(notes, [60, 61, 62, 63, 64, 65, 66, 68]);
    }

    #[test]
    fn a_switch_through_the_empty_buffer_keeps_the_release_tail() {
        // File A plays, then a still-loading file silences the engine (the empty buffer),
        // then file C arrives. A's tail keeps sounding throughout.
        let (mut cb, swap, mut tx) = callback(constant(1, 0.25));
        let mut retired = Vec::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        render(&mut cb, 64);
        tx.push(Command::AllOff).unwrap();
        retired.push(swap.swap(Arc::new(empty_buffer(RATE))));
        let mut out = left(&render(&mut cb, 100));
        retired.push(swap.swap(Arc::new(constant(3, -0.5))));
        out.extend(left(&render(&mut cb, 100)));
        assert!(out.iter().all(|s| *s > 0.0), "A's 10 ms release sounds on");
        let worst = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 0.25 / 480.0 + 1e-6, "a step of {worst}");
    }

    #[test]
    fn play_range_starts_a_voice_that_stops_at_the_end() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::PlayRange {
            note: 60,
            from: 100,
            to: 199,
        })
        .unwrap();
        let out = block(&mut state, &b, &mut rx, 100);
        assert!(out.iter().all(|s| (*s - 0.25).abs() < 1e-6));
        block(&mut state, &b, &mut rx, 480);
        assert!(active(&state).is_empty(), "released at frame 199");
    }

    #[test]
    fn switching_files_lets_released_voices_finish_on_their_own_audio() {
        // Starts like the engine, on the empty buffer, then opens file A.
        let (mut cb, swap, mut tx) = callback(empty_buffer(RATE));
        // Stands in for the UI's `retired` list: it keeps every swapped-out buffer alive.
        let mut retired = vec![swap.swap(Arc::new(constant(1, 0.25)))];
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        let out = render(&mut cb, 64);
        assert!(out.iter().all(|s| (*s - 0.25).abs() < 1e-6));

        tx.push(Command::AllOff).unwrap();
        retired.push(swap.swap(Arc::new(constant(2, -0.5))));
        let out = left(&render(&mut cb, 600));
        assert!(out.iter().all(|s| *s >= 0.0), "the tail never reads B");
        assert!(
            (out[0] - 0.25).abs() < 1e-6,
            "the tail starts from A: {}",
            out[0]
        );
        assert!(out[479] > 0.0 && out[479] < 0.001);
        assert!(out[480..].iter().all(|s| *s == 0.0), "and falls to 0");
        assert!(active(&cb.state).is_empty());

        // A new note plays the new file.
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        let out = render(&mut cb, 4);
        assert!(out.iter().all(|s| (*s + 0.5).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn live_edit_of_the_same_file_is_heard_at_once() {
        let (mut cb, swap, mut tx) = callback(constant(1, 0.25));
        let mut retired = Vec::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        render(&mut cb, 64);
        retired.push(swap.swap(Arc::new(constant(1, 0.75))));
        let out = render(&mut cb, 4);
        assert!(out.iter().all(|s| (*s - 0.75).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn a_voice_from_neither_buffer_is_dropped() {
        let (mut cb, swap, mut tx) = callback(constant(1, 0.25));
        let mut retired = Vec::new();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        render(&mut cb, 200);
        tx.push(Command::AllOff).unwrap();
        retired.push(swap.swap(Arc::new(constant(2, -0.5))));
        let out = render(&mut cb, 10);
        assert!(
            out.iter().all(|s| *s > 0.0),
            "A's 30 ms release is sounding"
        );
        retired.push(swap.swap(Arc::new(constant(3, -0.5))));
        let out = render(&mut cb, 10);
        assert!(out.iter().all(|s| *s == 0.0));
        assert!(active(&cb.state).is_empty());
    }

    #[test]
    fn repeating_the_same_envelope_does_not_delay_a_release() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 64);
        tx.push(Command::NoteOff { note: 60 }).unwrap();
        // As while a slider is dragged: the same envelope arrives every block.
        for _ in 0..48 {
            tx.push(Command::SetEnvelope(FLAT)).unwrap();
            block(&mut state, &b, &mut rx, 10);
        }
        assert!(
            active(&state).is_empty(),
            "died after its 480-frame release"
        );
    }

    #[test]
    fn saves_that_move_the_audio_release_held_voices_without_a_step() {
        // As `perform_save` does when a save gives the file a new source: `AllOff`, then the
        // new buffer. Two such saves in a row.
        let (mut cb, swap, mut tx) = callback(empty_buffer(RATE));
        let mut retired = vec![swap.swap(Arc::new(constant(2, 0.25)))];
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        let mut out = left(&render(&mut cb, 64));
        for source in [3, 4] {
            tx.push(Command::AllOff).unwrap();
            retired.push(swap.swap(Arc::new(constant(source, 0.25))));
            out.extend(left(&render(&mut cb, 600)));
        }
        assert!((out[0] - 0.25).abs() < 1e-6, "the note sounded");
        let worst = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 0.25 / 480.0 + 1e-6, "a step of {worst}");
        assert!(out.ends_with(&[0.0]), "the voice has finished");
    }

    #[test]
    fn engine_zero_channels_does_not_panic() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        process(
            &mut state,
            &b,
            &b,
            &mut rx,
            &Cursors::new(),
            &mut [],
            0,
            RATE,
        );
    }

    #[test]
    fn cursors_report_sounding_voices() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        let cursors = Cursors::new();
        assert!(cursors.read().is_empty());
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block_with(&mut state, &b, &mut rx, &cursors, 480);
        let got = cursors.read();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!((got[0].frame - 480.0).abs() <= 1.0, "{}", got[0].frame);
        assert_eq!(got[0].level, 1.0);
    }

    #[test]
    fn cursors_clear_when_voices_end() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        let cursors = Cursors::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        tx.push(Command::NoteOn { note: 64, from: 0 }).unwrap();
        block_with(&mut state, &b, &mut rx, &cursors, 64);
        assert_eq!(cursors.read().len(), 2);

        tx.push(Command::AllOff).unwrap();
        block_with(&mut state, &b, &mut rx, &cursors, 100);
        let fading = cursors.read();
        assert_eq!(fading.len(), 2, "still releasing");
        assert!(fading.iter().all(|c| c.level < 1.0));
        // The 10 ms (480 frame) release is over.
        block_with(&mut state, &b, &mut rx, &cursors, 480);
        assert!(cursors.read().is_empty());
    }

    #[test]
    fn an_emptied_slot_keeps_its_last_frame() {
        // Only the level is cleared, so a reader that saw the old level never pairs it with a
        // frame of 0.
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        let cursors = Cursors::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn {
            note: 60,
            from: 500,
        })
        .unwrap();
        block_with(&mut state, &b, &mut rx, &cursors, 64);
        tx.push(Command::AllOff).unwrap();
        block_with(&mut state, &b, &mut rx, &cursors, 600);
        assert!(cursors.read().is_empty());
        let frames: Vec<f64> = cursors
            .frames
            .iter()
            .map(|f| f64::from_bits(f.load(Ordering::Relaxed)))
            .collect();
        assert!(frames.iter().any(|f| *f > 500.0), "{frames:?}");
    }

    #[test]
    fn set_envelope_is_clamped_by_the_mixer() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        let wild = Adsr {
            attack_ms: -5.0,
            decay_ms: f32::NAN,
            sustain: 2.0,
            release_ms: f32::INFINITY,
        };
        tx.push(Command::SetEnvelope(wild)).unwrap();
        block(&mut state, &b, &mut rx, 16);
        assert_eq!(state.adsr, wild.clamped());
    }

    /// [`constant`] with 0.75 after the loop end, so a voice that leaves the loop shows it.
    fn with_release_audio() -> PlayBuffer {
        let mut b = constant(1, 0.25);
        for ch in &mut b.audio {
            ch[3000..].fill(0.75);
        }
        b
    }

    #[test]
    fn set_release_past_loop_reaches_new_voices() {
        let b = with_release_audio();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::SetEnvelope(Adsr {
            release_ms: 1000.0,
            ..FLAT
        }))
        .unwrap();
        // 72 (an octave up, two frames a frame) starts with the setting off and keeps it; 60
        // starts with it on.
        tx.push(Command::NoteOn { note: 72, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 16);
        tx.push(Command::SetReleasePastLoop(true)).unwrap();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 16);
        tx.push(Command::NoteOff { note: 72 }).unwrap();
        tx.push(Command::NoteOff { note: 60 }).unwrap();
        let out = left(&block(&mut state, &b, &mut rx, 4100));
        // 72 (at 64) passes the loop end at frame 1468 and wraps; 60 (at 16) reaches 3000 at
        // frame 2984 and dies after End at 3999.
        assert!(out[1500] < 0.5, "the old voice wraps: {}", out[1500]);
        assert!(out[2960] < 0.5, "both in the loop: {}", out[2960]);
        assert!(
            out[3010] > 0.9,
            "the new voice plays the release: {}",
            out[3010]
        );
        assert!(out[4050] < 0.5, "the new voice ended at End: {}", out[4050]);
        assert_eq!(active(&state), [72]);
    }

    #[test]
    fn preview_uses_the_attack_and_holds_after_note_off() {
        let b = with_release_audio();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        // The default envelope: a 2 ms attack and a 30 ms release.
        tx.push(Command::PreviewOn {
            note: 60,
            from: 2000,
            hold_loop: true,
        })
        .unwrap();
        let out = left(&block(&mut state, &b, &mut rx, 200));
        assert!(out[0] < 0.01, "the attack starts near silence");
        assert!((out[199] - 0.25).abs() < 1e-6);
        tx.push(Command::NoteOff { note: 60 }).unwrap();
        // 200 + 800 frames reach the loop end at 2999; the release audio plays at full level.
        let out = left(&block(&mut state, &b, &mut rx, 1500));
        assert!(out[..800].iter().all(|s| (*s - 0.25).abs() < 1e-6));
        assert!(out[800..].iter().all(|s| (*s - 0.75).abs() < 1e-6));
        assert_eq!(
            active(&state),
            [60],
            "still sounding 30 ms after the release"
        );
    }

    #[test]
    fn a_preview_without_hold_loop_plays_through_the_loop_to_end() {
        let b = with_release_audio();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        // From 2000, inside the loop (1000..=2999), with no note-off: it does not wrap.
        tx.push(Command::PreviewOn {
            note: 60,
            from: 2000,
            hold_loop: false,
        })
        .unwrap();
        let out = left(&block(&mut state, &b, &mut rx, 2500));
        assert!(out[200..1000].iter().all(|s| (*s - 0.25).abs() < 1e-6));
        assert!(
            out[1000..2000].iter().all(|s| (*s - 0.75).abs() < 1e-6),
            "past the loop end at full level"
        );
        assert!(out[2000..].iter().all(|s| *s == 0.0), "ended at End");
        assert!(active(&state).is_empty());

        // Started past the loop end, it plays on from there instead of jumping back.
        tx.push(Command::PreviewOn {
            note: 60,
            from: 3500,
            hold_loop: false,
        })
        .unwrap();
        let out = left(&block(&mut state, &b, &mut rx, 1000));
        assert!(out[200..500].iter().all(|s| (*s - 0.75).abs() < 1e-6));
        assert!(out[500..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn retriggering_a_preview_note_fades_the_preview_out() {
        let b = with_release_audio();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::PreviewOn {
            note: 60,
            from: 2000,
            hold_loop: true,
        })
        .unwrap();
        block(&mut state, &b, &mut rx, 200);
        // As "Play original" does while the preview holds on its loop.
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 480);
        assert_eq!(active(&state), [60], "the preview is gone within 10 ms");
        // Had it been released, it would reach the release audio (0.75) at frame 800.
        let out = left(&block(&mut state, &b, &mut rx, 1500));
        assert!(
            out.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "only the new note"
        );
    }

    #[test]
    fn preview_uses_the_attack_only() {
        let b = with_release_audio();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::SetEnvelope(Adsr {
            attack_ms: 0.0,
            decay_ms: 10.0,
            sustain: 0.5,
            release_ms: 30.0,
        }))
        .unwrap();
        tx.push(Command::PreviewOn {
            note: 60,
            from: 1000,
            hold_loop: true,
        })
        .unwrap();
        let out = left(&block(&mut state, &b, &mut rx, 3000));
        assert!(
            out.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "full level on the loop, without decay or sustain"
        );
    }

    #[test]
    fn stop_silences_a_preview_voice_within_10_ms() {
        let b = with_release_audio();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::PreviewOn {
            note: 60,
            from: 2000,
            hold_loop: true,
        })
        .unwrap();
        block(&mut state, &b, &mut rx, 400);
        tx.push(Command::AllOff).unwrap();
        let out = left(&block(&mut state, &b, &mut rx, 480));
        assert!(active(&state).is_empty(), "gone within 10 ms");
        assert!(out.windows(2).all(|w| w[1] <= w[0]), "a fade, not a cut");
        assert!(out[240..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn cursors_follow_backward_loops() {
        let mut b = looping();
        b.loop_mode = LoopMode::Backward;
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        let cursors = Cursors::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        // Forward from 1500 to the loop end at 2999, then backward.
        tx.push(Command::NoteOn {
            note: 60,
            from: 1500,
        })
        .unwrap();
        block_with(&mut state, &b, &mut rx, &cursors, 1600);
        let mut last = cursors.read()[0].frame;
        for _ in 0..5 {
            block_with(&mut state, &b, &mut rx, &cursors, 100);
            let now = cursors.read()[0].frame;
            assert!(now < last - 90.0, "{now} should be below {last}");
            last = now;
        }
    }
}
