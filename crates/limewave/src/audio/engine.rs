//! Real-time audio output: a fixed voice mixer fed by lock-free command queues.
//!
//! The audio callback never allocates, locks, blocks, prints or panics. The UI publishes a new
//! [`PlayBuffer`] through an [`ArcSwap`] and sends [`Command`]s through `rtrb` ring buffers.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use arc_swap::{ArcSwap, ArcSwapOption};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, I24, SampleFormat, SizedSample, StreamConfig, U24};
use limewave_core::edit::LoopMode;

use super::voice::{Adsr, InstrumentPlay, PlayBuffer, STEAL_FADE_MS, Voice, velocity_gain};

pub use super::voice::MAX_VOICES;

/// Capacity of each command ring buffer.
const COMMAND_CAPACITY: usize = 256;
/// Frames of the conversion scratch buffer used for non-`f32` devices.
const SCRATCH_FRAMES: usize = 16_384;

/// A message from the UI or MIDI thread to the audio callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    /// Plays `note` from frame `from` of the current buffer. With an instrument set it plays
    /// every region matching `note` and `velocity` instead, each from its own start
    /// ([`AudioEngine::set_instrument`]); without one `velocity` is ignored.
    NoteOn {
        note: u8,
        from: usize,
        velocity: u8,
    },
    /// Plays `note` from frame `from` of the current buffer, like `NoteOn` without an
    /// instrument, even when one is set: the Play button always plays the shown sample.
    PlaySample {
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

/// Where one sounding voice is: its read position in file frames, its envelope level and the
/// source it plays ([`PlayBuffer::source`]), which tells whose frames those are.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cursor {
    pub frame: f64,
    pub level: f32,
    pub source: u64,
}

/// The voices' positions, published by the audio thread for the UI to draw as cursors.
///
/// One slot per voice: the `f64` bits of the frame, the source and the `f32` bits of the level,
/// where a level of 0 marks an empty slot. The audio thread writes every slot once per callback
/// with plain atomic stores (no allocation, no locks). The frame and source are stored
/// `Relaxed` and the level with `Release`; an empty slot stores only its level, keeping its
/// last frame and source. [`Cursors::read`] loads the level with `Acquire` before the frame
/// and source, so a cursor never pairs a new level with an older frame or source. Slots are
/// independent: a read may mix slots from two consecutive callbacks, which at most moves one
/// cursor by a few milliseconds.
pub struct Cursors {
    frames: [AtomicU64; MAX_VOICES],
    sources: [AtomicU64; MAX_VOICES],
    levels: [AtomicU32; MAX_VOICES],
}

impl Cursors {
    pub fn new() -> Cursors {
        Cursors {
            frames: [const { AtomicU64::new(0) }; MAX_VOICES],
            sources: [const { AtomicU64::new(0) }; MAX_VOICES],
            levels: [const { AtomicU32::new(0) }; MAX_VOICES],
        }
    }

    /// The cursors of the voices currently sounding. UI side; allocates.
    pub fn read(&self) -> Vec<Cursor> {
        self.levels
            .iter()
            .zip(&self.frames)
            .zip(&self.sources)
            .filter_map(|((level, frame), source)| {
                let level = f32::from_bits(level.load(Ordering::Acquire));
                (level > 0.0).then(|| Cursor {
                    frame: f64::from_bits(frame.load(Ordering::Relaxed)),
                    level,
                    source: source.load(Ordering::Relaxed),
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
                self.sources[i].store(v.source(), Ordering::Relaxed);
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

/// Spare slots where stolen voices fade out. One note-on can steal a voice per layer.
const DYING_VOICES: usize = MAX_VOICES / 2;

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

    fn apply(&mut self, cmd: Command, sources: &Sources, device_rate: u32) {
        let buf = sources.current;
        match cmd {
            Command::NoteOn {
                note,
                from,
                velocity,
            } => match sources.instrument {
                Some(instrument) => self.regions_on(note, velocity, instrument, device_rate),
                None => self.note_on(note, from, None, None, buf, device_rate),
            },
            Command::PlaySample { note, from } => {
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
            // A one-shot ignores note-off, so Stop fades it out like a preview.
            Command::AllOff => {
                for v in self.voices.iter_mut().flatten() {
                    if v.is_preview() || v.is_one_shot() {
                        v.fade_out(STEAL_FADE_MS);
                    } else {
                        v.release();
                    }
                }
            }
            // Instrument voices keep their region's envelope.
            Command::SetEnvelope(a) => {
                let a = a.clamped();
                self.adsr = a;
                for v in self.voices.iter_mut().flatten() {
                    if !v.is_instrument() {
                        v.set_release_ms(a.release_ms);
                    }
                }
            }
        }
    }

    /// Releases any voice already playing `note` (a preview fades out), then starts a new one.
    /// `preview` (`Some(hold_loop)`) starts a [`Command::PreviewOn`] voice.
    fn note_on(
        &mut self,
        note: u8,
        from: usize,
        stop_at: Option<usize>,
        preview: Option<bool>,
        buf: &PlayBuffer,
        device_rate: u32,
    ) {
        self.release_note(note);
        let voice = Voice::new(note, from, buf, device_rate, self.adsr, stop_at)
            .release_past_loop(self.release_past_loop);
        self.start(match preview {
            Some(hold_loop) => voice.preview(hold_loop),
            None => voice,
        });
    }

    /// Releases any voice already playing `note`, then starts one voice for every region of
    /// `instrument` matching `note` and `velocity` (spec §6.1), each from its region's start
    /// with the region's envelope, playback mode and level.
    fn regions_on(&mut self, note: u8, velocity: u8, instrument: &InstrumentPlay, rate: u32) {
        self.release_note(note);
        for r in instrument
            .regions
            .iter()
            .filter(|r| r.matches(note, velocity))
        {
            let b = &r.buffer;
            let gain = r.gain * velocity_gain(velocity, r.veltrack);
            let voice = Voice::new(note, b.start, b, rate, r.adsr.clamped(), None)
                .for_region(gain, r.playback);
            self.start(voice);
        }
    }

    /// Releases every voice playing `note`. A preview released here would play its whole held
    /// release under the new note, so it fades out instead; a one-shot plays on.
    fn release_note(&mut self, note: u8) {
        for v in self
            .voices
            .iter_mut()
            .flatten()
            .filter(|v| v.note() == note)
        {
            if v.is_preview() {
                v.fade_out(STEAL_FADE_MS);
            } else {
                v.release();
            }
        }
    }

    /// Puts `voice` in a free slot. With none free a voice is stolen: a released one playing
    /// the same note, else the oldest releasing voice, else the oldest. The stolen voice fades
    /// out over [`STEAL_FADE_MS`] in a spare slot rather than being cut.
    fn start(&mut self, voice: Voice) {
        let slot = match self.voices.iter().position(Option::is_none) {
            Some(free) => free,
            None => {
                let note = voice.note();
                let victim = self
                    .voices
                    .iter()
                    .rposition(|v| {
                        v.as_ref()
                            .is_some_and(|v| v.note() == note && v.is_releasing())
                    })
                    .unwrap_or_else(|| self.victim());
                self.retire(victim);
                victim
            }
        };
        self.voices[slot] = Some(voice);
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

/// Everything the voices can play: the buffers and instrument maps the callback holds.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    /// The buffer new notes play when no instrument is set, and play ranges and previews
    /// always.
    pub current: &'a PlayBuffer,
    pub previous: &'a PlayBuffer,
    /// The instrument new notes play, if one is set.
    pub instrument: Option<&'a InstrumentPlay>,
    pub previous_instrument: Option<&'a InstrumentPlay>,
}

impl<'a> Sources<'a> {
    /// The buffer `v` plays: for an instrument voice the region with its source in the
    /// current then the previous instrument, else whichever of the two buffers has it. Does
    /// not allocate.
    fn find(&self, v: &Voice) -> Option<&'a PlayBuffer> {
        if v.is_instrument() {
            [self.instrument, self.previous_instrument]
                .into_iter()
                .flatten()
                .flat_map(|m| &m.regions)
                .map(|r| &r.buffer)
                .find(|b| b.source == v.source())
        } else {
            [self.current, self.previous]
                .into_iter()
                .find(|b| b.source == v.source())
        }
    }
}

/// The body of the audio callback: applies pending commands (new notes play the current
/// instrument, else the current buffer), then fills interleaved `out` (cleared first) with the
/// mixed voices, stolen ones still fading included, clamped to [-1, 1]. Each voice plays
/// the buffer (or, for an instrument voice, the region) in `sources` with its source; a voice
/// whose source is in none is dropped. Afterwards the voices' positions are published to
/// `cursors`. Does not allocate.
pub fn process(
    state: &mut MixerState,
    sources: &Sources,
    cmds: &mut rtrb::Consumer<Command>,
    cursors: &Cursors,
    out: &mut [f32],
    channels: usize,
    device_rate: u32,
) {
    while let Ok(cmd) = cmds.pop() {
        state.apply(cmd, sources, device_rate);
    }
    out.fill(0.0);
    if channels > 0 {
        for slot in state.voices.iter_mut().chain(&mut state.dying) {
            let Some(v) = slot else { continue };
            let buf = sources.find(v);
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
    instrument: Arc<ArcSwapOption<InstrumentPlay>>,
    /// The instrument last loaded, and the last one with a different source before it, which
    /// voices released on the previous instrument finish on. Like the buffers, never the last
    /// reference: `AudioEngine::set_instrument` retires every instrument it swaps out.
    current_instrument: Option<Arc<InstrumentPlay>>,
    previous_instrument: Option<Arc<InstrumentPlay>>,
    ui_rx: rtrb::Consumer<Command>,
    midi_rx: rtrb::Consumer<Command>,
    cursors: Arc<Cursors>,
    channels: usize,
    device_rate: u32,
}

impl Callback {
    fn new(
        buffer: Arc<ArcSwap<PlayBuffer>>,
        instrument: Arc<ArcSwapOption<InstrumentPlay>>,
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
            current_instrument: instrument.load_full(),
            previous_instrument: None,
            instrument,
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
        // The same for the instrument; no instrument plays the part of the empty buffer.
        let loaded = self.instrument.load_full();
        let source = |m: &Option<Arc<InstrumentPlay>>| m.as_ref().map(|m| m.source);
        if source(&loaded) == source(&self.current_instrument) {
            self.current_instrument = loaded;
        } else {
            let old = std::mem::replace(&mut self.current_instrument, loaded);
            if old.is_some() {
                self.previous_instrument = old;
            }
        }
        let sources = Sources {
            current: &self.current,
            previous: &self.previous,
            instrument: self.current_instrument.as_deref(),
            previous_instrument: self.previous_instrument.as_deref(),
        };
        while let Ok(cmd) = self.midi_rx.pop() {
            self.state.apply(cmd, &sources, self.device_rate);
        }
        process(
            &mut self.state,
            &sources,
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
    instrument: Arc<ArcSwapOption<InstrumentPlay>>,
    /// Instruments swapped out by `set_instrument`, kept like `retired`.
    retired_instruments: RefCell<Vec<Arc<InstrumentPlay>>>,
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
        let instrument = Arc::new(ArcSwapOption::empty());
        let cursors = Arc::new(Cursors::new());
        let cb = Callback::new(
            Arc::clone(&buffer),
            Arc::clone(&instrument),
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
            instrument,
            retired_instruments: RefCell::new(Vec::new()),
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

    /// Publishes the instrument new notes play, or `None` to play the buffer again. Voices
    /// already sounding finish on the instrument they started on (spec §6).
    pub fn set_instrument(&self, instrument: Option<InstrumentPlay>) {
        let old = self.instrument.swap(instrument.map(Arc::new));
        let mut retired = self.retired_instruments.borrow_mut();
        retired.extend(old);
        // As in `set_buffer`: a count of 1 means only this list holds it.
        retired.retain(|m| Arc::strong_count(m) > 1);
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
    let instrument = Arc::new(ArcSwapOption::empty());
    let cursors = Arc::new(Cursors::new());
    let cb = Callback::new(
        Arc::clone(&buffer),
        Arc::clone(&instrument),
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
        instrument,
        retired_instruments: RefCell::new(Vec::new()),
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

    /// The instrument the callback plays new notes on, as of the last render.
    pub(crate) fn instrument(&self) -> Option<&InstrumentPlay> {
        self.0.current_instrument.as_deref()
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
        audio: Arc::default(),
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
    use super::super::voice::{Adsr, Playback, RegionPlay, ampeg_to_adsr, velocity_gain};
    use super::*;
    use limewave_core::edit::LoopMode;
    use limewave_core::sfz::Ampeg;

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
            audio: Arc::new(vec![vec![value; 4000], vec![value; 4000]]),
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

    /// Notes 60 up to (not including) this one fill every voice slot.
    const FULL: u8 = 60 + MAX_VOICES as u8;

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
        process(state, &plain(b), rx, cursors, &mut out, 2, RATE);
        out
    }

    /// Only `b` to play, and no instrument.
    fn plain(b: &PlayBuffer) -> Sources<'_> {
        Sources {
            current: b,
            previous: b,
            instrument: None,
            previous_instrument: None,
        }
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
        let cb = Callback::new(
            Arc::clone(&swap),
            Arc::default(),
            Arc::default(),
            rx,
            midi_rx,
            2,
            RATE,
        );
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

        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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

        // One note more than there are voices: the oldest (60) is stolen.
        for note in 60..=FULL {
            tx.push(Command::NoteOn {
                note,
                from: 0,
                velocity: 100,
            })
            .unwrap();
        }
        let out = block(&mut state, &b, &mut rx, 64);
        let mut notes = active(&state);
        notes.sort_unstable();
        assert_eq!(notes, (61..=FULL).collect::<Vec<u8>>());
        // The voices of 0.25 sum to more than 1; the output is clamped.
        assert!(out.iter().all(|s| *s == 1.0));
    }

    #[test]
    fn all_off_releases_through_the_envelope() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        // The default envelope: a 2 ms attack and a 30 ms (1440 frame) release.
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        tx.push(Command::NoteOn {
            note: 64,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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

        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        block(&mut state, &b, &mut rx, 64);
        // A free slot exists: the old voice fades while the new one starts.
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        block(&mut state, &b, &mut rx, 1);
        assert_eq!(active(&state), [60, 60]);
        block(&mut state, &b, &mut rx, 480);
        assert_eq!(active(&state), [60]);

        // All slots busy: retriggering restarts the note in place instead of stealing.
        for note in 61..FULL {
            tx.push(Command::NoteOn {
                note,
                from: 0,
                velocity: 100,
            })
            .unwrap();
        }
        tx.push(Command::NoteOn {
            note: 61,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        block(&mut state, &b, &mut rx, 1);
        let mut notes = active(&state);
        notes.sort_unstable();
        assert_eq!(notes, (60..FULL).collect::<Vec<u8>>());
    }

    #[test]
    fn a_stolen_voice_fades_out_instead_of_cutting() {
        // Every voice at 0.05 stays below the output clamp, so every step shows.
        let b = constant(1, 0.05);
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
        for note in 60..FULL {
            tx.push(Command::NoteOn {
                note,
                from: 0,
                velocity: 100,
            })
            .unwrap();
        }
        let mut out = left(&block(&mut state, &b, &mut rx, 400));
        tx.push(Command::NoteOn {
            note: FULL,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        out.extend(left(&block(&mut state, &b, &mut rx, 600)));
        assert!(
            (out[399] - 0.8).abs() < 1e-5,
            "every voice held: {}",
            out[399]
        );

        let worst = out[300..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 2.0 * 0.05 / 240.0 + 1e-6, "a step of {worst}");
        let mut notes = active(&state);
        notes.sort_unstable();
        assert_eq!(notes, (61..=FULL).collect::<Vec<u8>>());
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
        // 60 is the oldest and still held; the newest is releasing.
        for note in 60..FULL {
            tx.push(Command::NoteOn {
                note,
                from: 0,
                velocity: 100,
            })
            .unwrap();
        }
        tx.push(Command::NoteOff { note: FULL - 1 }).unwrap();
        block(&mut state, &b, &mut rx, 64);
        tx.push(Command::NoteOn {
            note: FULL,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        block(&mut state, &b, &mut rx, 1);
        let mut notes = active(&state);
        notes.sort_unstable();
        let want: Vec<u8> = (60..FULL - 1).chain([FULL]).collect();
        assert_eq!(notes, want);
    }

    #[test]
    fn a_switch_through_the_empty_buffer_keeps_the_release_tail() {
        // File A plays, then a still-loading file silences the engine (the empty buffer),
        // then file C arrives. A's tail keeps sounding throughout.
        let (mut cb, swap, mut tx) = callback(constant(1, 0.25));
        let mut retired = Vec::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        let out = render(&mut cb, 4);
        assert!(out.iter().all(|s| (*s + 0.5).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn live_edit_of_the_same_file_is_heard_at_once() {
        let (mut cb, swap, mut tx) = callback(constant(1, 0.25));
        let mut retired = Vec::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        render(&mut cb, 64);
        retired.push(swap.swap(Arc::new(constant(1, 0.75))));
        let out = render(&mut cb, 4);
        assert!(out.iter().all(|s| (*s - 0.75).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn a_voice_from_neither_buffer_is_dropped() {
        let (mut cb, swap, mut tx) = callback(constant(1, 0.25));
        let mut retired = Vec::new();
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        process(
            &mut state,
            &plain(&b),
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        block_with(&mut state, &b, &mut rx, &cursors, 480);
        let got = cursors.read();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!((got[0].frame - 480.0).abs() <= 1.0, "{}", got[0].frame);
        assert_eq!(got[0].level, 1.0);
        assert_eq!(got[0].source, b.source);
    }

    #[test]
    fn cursors_clear_when_voices_end() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        let cursors = Cursors::new();
        tx.push(Command::SetEnvelope(FLAT)).unwrap();
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        tx.push(Command::NoteOn {
            note: 64,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
            velocity: 100,
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
        for ch in Arc::make_mut(&mut b.audio) {
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
        tx.push(Command::NoteOn {
            note: 72,
            from: 0,
            velocity: 100,
        })
        .unwrap();
        block(&mut state, &b, &mut rx, 16);
        tx.push(Command::SetReleasePastLoop(true)).unwrap();
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
        tx.push(Command::NoteOn {
            note: 60,
            from: 0,
            velocity: 100,
        })
        .unwrap();
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
            velocity: 100,
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

    /// A region over `keys` and `vels` playing `buffer` at its own level, with the flat
    /// envelope and no velocity tracking.
    fn region(buffer: PlayBuffer, keys: (u8, u8), vels: (u8, u8)) -> RegionPlay {
        RegionPlay {
            buffer,
            lokey: keys.0,
            hikey: keys.1,
            lovel: vels.0,
            hivel: vels.1,
            gain: 1.0,
            veltrack: 0.0,
            playback: Playback::Normal,
            adsr: FLAT,
        }
    }

    fn instrument(source: u64, regions: Vec<RegionPlay>) -> Option<InstrumentPlay> {
        Some(InstrumentPlay { regions, source })
    }

    fn note_on(note: u8, velocity: u8) -> Command {
        Command::NoteOn {
            note,
            from: 0,
            velocity,
        }
    }

    fn sorted_sources(out: &HeadlessOutput) -> Vec<u64> {
        let mut sources = out.voice_sources();
        sources.sort_unstable();
        sources
    }

    #[test]
    fn a_note_plays_every_matching_region() {
        let (mut engine, mut out) = headless(RATE);
        engine.set_instrument(instrument(
            100,
            vec![
                region(constant(11, 0.1), (0, 127), (1, 127)),
                region(constant(12, 0.2), (0, 127), (1, 127)),
                region(constant(13, 0.4), (70, 80), (1, 127)),
            ],
        ));
        engine.send(note_on(60, 100));
        let got = left(&out.render(64));
        assert!(
            got.iter().all(|s| (*s - 0.3).abs() < 1e-6),
            "{:?}",
            &got[..4]
        );
        assert_eq!(sorted_sources(&out), [11, 12]);
    }

    #[test]
    fn a_region_voice_starts_at_the_region_start() {
        let (mut engine, mut out) = headless(RATE);
        // 0.75 after frame 3000: a voice honouring `from` would read it.
        let mut b = with_release_audio();
        b.source = 11;
        b.start = 500;
        engine.set_instrument(instrument(100, vec![region(b, (0, 127), (1, 127))]));
        engine.send(Command::NoteOn {
            note: 60,
            from: 3500,
            velocity: 100,
        });
        let got = left(&out.render(100));
        assert!(
            got.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "{:?}",
            &got[..4]
        );
        let cursors = engine.cursors();
        assert_eq!(cursors.len(), 1);
        assert_eq!(cursors[0].frame, 600.0);
    }

    #[test]
    fn velocity_selects_layers() {
        let (mut engine, mut out) = headless(RATE);
        engine.set_instrument(instrument(
            100,
            vec![
                region(constant(11, 0.1), (0, 127), (1, 63)),
                region(constant(12, 0.2), (0, 127), (64, 127)),
            ],
        ));
        engine.send(note_on(60, 100));
        let got = left(&out.render(64));
        assert!(
            got.iter().all(|s| (*s - 0.2).abs() < 1e-6),
            "{:?}",
            &got[..4]
        );
        assert_eq!(out.voice_sources(), [12]);
        engine.send(note_on(62, 30));
        out.render(1);
        assert_eq!(sorted_sources(&out), [11, 12]);
    }

    #[test]
    fn velocity_gain_curve() {
        assert_eq!(velocity_gain(127, 1.0), 1.0);
        assert!((velocity_gain(64, 1.0) - (64.0f32 / 127.0).powi(2)).abs() < 1e-6);
        assert_eq!(velocity_gain(10, 0.0), 1.0);
        // Half way in dB: half the full curve's dB.
        assert!((velocity_gain(64, 0.5) - 64.0 / 127.0).abs() < 1e-6);
        assert_eq!(velocity_gain(0, 1.0), 0.0);
        assert_eq!(velocity_gain(0, 0.5), 0.0);
        assert_eq!(velocity_gain(0, 0.0), 1.0);
    }

    #[test]
    fn velocity_tracking_scales_a_region() {
        let (mut engine, mut out) = headless(RATE);
        let mut r = region(constant(11, 0.5), (0, 127), (1, 127));
        r.veltrack = 1.0;
        engine.set_instrument(instrument(100, vec![r]));
        engine.send(note_on(60, 64));
        let got = left(&out.render(16));
        let want = 0.5 * (64.0f32 / 127.0).powi(2);
        assert!(
            got.iter().all(|s| (*s - want).abs() < 1e-6),
            "{:?}",
            &got[..4]
        );
    }

    /// The peak of the left channel while `r` plays note 60 for 1000 frames.
    fn region_peak(r: RegionPlay) -> f32 {
        let (mut engine, mut out) = headless(RATE);
        engine.set_instrument(instrument(100, vec![r]));
        engine.send(note_on(60, 100));
        left(&out.render(1000))
            .iter()
            .fold(0.0f32, |p, s| p.max(s.abs()))
    }

    #[test]
    fn region_gain_scales_output() {
        let full = region_peak(region(constant(11, 0.5), (0, 127), (1, 127)));
        let mut quiet = region(constant(11, 0.5), (0, 127), (1, 127));
        // `volume=-6`.
        quiet.gain = 10f32.powf(-6.0 / 20.0);
        let ratio = region_peak(quiet) / full;
        assert!((ratio - 0.5).abs() <= 0.005, "ratio {ratio}");
    }

    #[test]
    fn one_shot_ignores_note_off_but_not_all_off() {
        let (mut engine, mut out) = headless(RATE);
        let mut r = region(constant(11, 0.25), (0, 127), (1, 127));
        r.playback = Playback::OneShot;
        engine.set_instrument(instrument(100, vec![r]));
        engine.send(note_on(60, 100));
        out.render(100);
        engine.send(Command::NoteOff { note: 60 });
        // Far longer than the 10 ms release.
        let got = left(&out.render(1000));
        assert!(
            got.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "still at full level"
        );
        assert_eq!(out.voice_sources(), [11]);
        // Stop fades it out.
        engine.send(Command::AllOff);
        let got = left(&out.render(480));
        assert!(got.windows(2).all(|w| w[1] <= w[0]), "a fade, not a cut");
        assert!(out.voice_sources().is_empty(), "silenced by Stop");
        assert!(left(&out.render(16)).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn one_shot_plays_through_the_loop_to_end() {
        let (mut engine, mut out) = headless(RATE);
        let mut b = with_release_audio();
        b.source = 11;
        let mut r = region(b, (0, 127), (1, 127));
        r.playback = Playback::OneShot;
        engine.set_instrument(instrument(100, vec![r]));
        engine.send(note_on(60, 100));
        // Held throughout: past the loop end at 2999 without wrapping, then it ends at 3999.
        let got = left(&out.render(4100));
        assert!(got[..3000].iter().all(|s| (*s - 0.25).abs() < 1e-6));
        assert!(got[3000..4000].iter().all(|s| (*s - 0.75).abs() < 1e-6));
        assert!(got[4000..].iter().all(|s| *s == 0.0));
        assert!(out.voice_sources().is_empty());
    }

    /// Plays note 60 on a region with `playback`, holds it 16 frames and releases it
    /// (a 1 s release), with the session's "Release plays after loop end" set to `session`.
    fn release_region(playback: Playback, session: bool) -> (Vec<f32>, Vec<u64>) {
        let (mut engine, mut out) = headless(RATE);
        let mut b = with_release_audio();
        b.source = 11;
        let mut r = region(b, (0, 127), (1, 127));
        r.playback = playback;
        r.adsr.release_ms = 1000.0;
        engine.set_instrument(instrument(100, vec![r]));
        engine.send(Command::SetReleasePastLoop(session));
        engine.send(note_on(60, 100));
        out.render(16);
        engine.send(Command::NoteOff { note: 60 });
        let got = left(&out.render(4100));
        (got, out.voice_sources())
    }

    #[test]
    fn sustain_region_leaves_loop_on_release() {
        let (got, voices) = release_region(Playback::Sustain, false);
        // From frame 16, it reaches 3000 after 2984 frames and End after 3984.
        assert!(got[2990] > 0.5, "plays past the loop end: {}", got[2990]);
        assert!(got[4000..].iter().all(|s| *s == 0.0), "ended at End");
        assert!(voices.is_empty());

        // A Normal region stays in its loop, whatever the session setting.
        let (got, voices) = release_region(Playback::Normal, true);
        assert!(got.iter().all(|s| *s < 0.5), "never leaves the loop");
        assert_eq!(voices, [11], "the 1 s release is still sounding");
    }

    #[test]
    fn region_voices_keep_their_own_envelope() {
        let (mut engine, mut out) = headless(RATE);
        let mut r = region(constant(11, 0.25), (0, 127), (1, 127));
        r.adsr.release_ms = 1000.0;
        engine.set_instrument(instrument(100, vec![r]));
        engine.send(note_on(60, 100));
        out.render(16);
        // The session envelope's 10 ms release does not reach the region's voice.
        engine.set_envelope(FLAT);
        engine.send(Command::NoteOff { note: 60 });
        out.render(1000);
        assert_eq!(out.voice_sources(), [11]);
    }

    #[test]
    fn layers_beyond_voice_count_steal_not_stick() {
        let (mut engine, mut out) = headless(RATE);
        engine.set_instrument(instrument(
            100,
            vec![
                region(constant(11, 0.01), (0, 127), (1, 127)),
                region(constant(12, 0.02), (0, 127), (1, 127)),
            ],
        ));
        for note in 60..72 {
            engine.send(note_on(note, 100));
        }
        out.render(64);
        let sources = sorted_sources(&out);
        assert_eq!(sources.len(), MAX_VOICES, "every slot busy");
        // The newest eight notes kept both layers.
        assert_eq!(sources.iter().filter(|s| **s == 11).count(), 8);
        for note in 60..72 {
            engine.send(Command::NoteOff { note });
        }
        // The 10 ms (480 frame) release.
        out.render(480);
        assert!(out.voice_sources().is_empty(), "no voice left held");
        assert!(left(&out.render(64)).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_released_region_finishes_on_the_previous_map() {
        let (mut engine, mut out) = headless(RATE);
        let mut r = region(constant(11, 0.25), (0, 127), (1, 127));
        r.adsr.release_ms = 1000.0;
        engine.set_instrument(instrument(100, vec![r]));
        engine.send(note_on(60, 100));
        out.render(16);
        engine.send(Command::NoteOff { note: 60 });
        // Another instrument, as when a different one is opened.
        engine.set_instrument(instrument(
            200,
            vec![region(constant(21, -0.5), (0, 127), (1, 127))],
        ));
        let got = left(&out.render(100));
        assert!(got.iter().all(|s| *s > 0.2), "A's release sounds on");
        assert_eq!(out.voice_sources(), [11]);
        assert_eq!(engine.retired_instruments.borrow().len(), 1, "A is kept");
        // A third instrument pushes A out of the callback, and its voice with it.
        engine.set_instrument(instrument(300, Vec::new()));
        out.render(16);
        assert!(out.voice_sources().is_empty());
        // Only the retired list held A then, so the next swap frees it here.
        engine.set_instrument(None);
        assert!(
            engine
                .retired_instruments
                .borrow()
                .iter()
                .all(|m| m.source != 100)
        );
    }

    #[test]
    fn without_an_instrument_velocity_is_ignored() {
        let (mut engine, mut out) = headless(RATE);
        engine.set_buffer(constant(1, 0.25));
        engine.set_envelope(FLAT);
        engine.set_instrument(instrument(
            100,
            vec![region(constant(11, 0.5), (0, 127), (1, 127))],
        ));
        engine.set_instrument(None);
        engine.send(note_on(60, 1));
        let got = left(&out.render(64));
        assert!(
            got.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "{:?}",
            &got[..4]
        );
        assert_eq!(out.voice_sources(), [1]);
    }

    #[test]
    fn play_range_and_previews_ignore_the_instrument() {
        let (mut engine, mut out) = headless(RATE);
        engine.set_buffer(constant(1, 0.25));
        engine.set_envelope(FLAT);
        engine.set_instrument(instrument(
            100,
            vec![region(constant(11, 0.5), (0, 127), (1, 127))],
        ));
        engine.send(Command::PlayRange {
            note: 60,
            from: 0,
            to: 3000,
        });
        engine.send(Command::PreviewOn {
            note: 62,
            from: 0,
            hold_loop: true,
        });
        out.render(16);
        assert_eq!(out.voice_sources(), [1, 1]);
    }
    #[test]
    fn a_region_with_no_release_still_fades() {
        let (mut engine, mut out) = headless(RATE);
        let mut r = region(constant(11, 0.25), (0, 127), (1, 127));
        // The SFZ defaults: `ampeg_release=0`.
        r.adsr = ampeg_to_adsr(&Ampeg {
            attack: 0.0,
            decay: 0.0,
            sustain: 100.0,
            release: 0.0,
        });
        engine.set_instrument(instrument(100, vec![r]));
        engine.send(note_on(60, 100));
        out.render(100);
        engine.send(Command::NoteOff { note: 60 });
        let got = left(&out.render(300));
        assert!(got[..240].iter().all(|s| *s > 0.0), "sounds for 5 ms");
        let worst = got
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 0.25 / 240.0 + 1e-6, "a step of {worst}");
        assert!(got[240..].iter().all(|s| *s == 0.0));
        assert!(out.voice_sources().is_empty());
    }

    #[test]
    fn play_sample_ignores_the_instrument() {
        let (mut engine, mut out) = headless(RATE);
        engine.set_buffer(constant(1, 0.25));
        engine.set_envelope(FLAT);
        engine.set_instrument(instrument(
            100,
            vec![region(constant(11, 0.5), (0, 127), (1, 127))],
        ));
        engine.send(Command::PlaySample { note: 60, from: 0 });
        let got = left(&out.render(64));
        assert!(
            got.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "{:?}",
            &got[..4]
        );
        assert_eq!(out.voice_sources(), [1]);
    }

    #[test]
    fn voices_look_for_their_source_only_among_their_own_kind() {
        let (mut engine, mut out) = headless(RATE);
        engine.set_buffer(constant(1, 0.25));
        engine.set_envelope(FLAT);
        // A region whose source is the buffer's too.
        engine.set_instrument(instrument(
            100,
            vec![region(constant(1, 0.5), (60, 60), (1, 127))],
        ));
        engine.send(note_on(60, 100));
        let got = left(&out.render(64));
        assert!(got.iter().all(|s| (*s - 0.5).abs() < 1e-6), "the region");
        engine.send(Command::AllOff);
        out.render(600);
        engine.send(Command::PlaySample { note: 60, from: 0 });
        let got = left(&out.render(64));
        assert!(got.iter().all(|s| (*s - 0.25).abs() < 1e-6), "the buffer");
    }

    #[test]
    fn stolen_layers_fade_instead_of_cutting() {
        let (mut engine, mut out) = headless(RATE);
        // Eight layers of 0.04 with a 5 ms (240 frame) attack: two notes fill every slot.
        let layers = (0..8)
            .map(|i| {
                let mut r = region(constant(11 + i, 0.04), (0, 127), (1, 127));
                r.adsr = Adsr {
                    attack_ms: 5.0,
                    decay_ms: 0.0,
                    sustain: 1.0,
                    release_ms: 1000.0,
                };
                r
            })
            .collect();
        engine.set_instrument(instrument(100, layers));
        engine.send(note_on(60, 100));
        engine.send(note_on(61, 100));
        let mut got = left(&out.render(300));
        assert!((got[299] - 0.64).abs() < 1e-5, "sixteen held voices");
        // The third note steals all eight of the first note's voices at full level.
        engine.send(note_on(62, 100));
        got.extend(left(&out.render(600)));
        let worst = got[250..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 2.0 * 0.32 / 240.0 + 1e-6, "a step of {worst}");
        assert!((got[899] - 0.64).abs() < 1e-5, "the stolen voices are gone");
    }
}
