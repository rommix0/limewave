//! Real-time audio output: a fixed voice mixer fed by lock-free command queues.
//!
//! The audio callback never allocates, locks, blocks, prints or panics. The UI publishes a new
//! [`PlayBuffer`] through an [`ArcSwap`] and sends [`Command`]s through `rtrb` ring buffers.

use std::cell::RefCell;
use std::sync::Arc;

use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, I24, SampleFormat, SizedSample, StreamConfig, U24};
use limewave_core::edit::LoopMode;

use super::voice::{MAX_VOICES, PlayBuffer, Voice};

/// Capacity of each command ring buffer.
const COMMAND_CAPACITY: usize = 256;
/// Frames of the conversion scratch buffer used for non-`f32` devices.
const SCRATCH_FRAMES: usize = 16_384;

/// A message from the UI or MIDI thread to the audio callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    NoteOn { note: u8, from: usize },
    NoteOff { note: u8 },
    AllOff,
}

/// Voice slots owned by the audio callback.
pub struct MixerState {
    voices: [Option<Voice>; MAX_VOICES],
    age: [u64; MAX_VOICES],
    counter: u64,
}

impl MixerState {
    pub fn new() -> MixerState {
        MixerState {
            voices: [const { None }; MAX_VOICES],
            age: [0; MAX_VOICES],
            counter: 0,
        }
    }

    fn apply(&mut self, cmd: Command, buf: &PlayBuffer, device_rate: u32) {
        match cmd {
            Command::NoteOn { note, from } => self.note_on(note, from, buf, device_rate),
            Command::NoteOff { note } => self
                .voices
                .iter_mut()
                .flatten()
                .filter(|v| v.note() == note)
                .for_each(Voice::release),
            Command::AllOff => self.voices.iter_mut().flatten().for_each(Voice::release),
        }
    }

    /// Releases any voice already playing `note`, then starts a new one in a free slot. With no
    /// free slot the sounding voice is restarted in place, or else the oldest voice is stolen.
    fn note_on(&mut self, note: u8, from: usize, buf: &PlayBuffer, device_rate: u32) {
        let mut existing = None;
        for (i, slot) in self.voices.iter_mut().enumerate() {
            if let Some(v) = slot.as_mut().filter(|v| v.note() == note) {
                v.release();
                existing = Some(i);
            }
        }
        let slot = self
            .voices
            .iter()
            .position(Option::is_none)
            .or(existing)
            .unwrap_or_else(|| self.oldest());
        self.voices[slot] = Some(Voice::new(note, from, buf, device_rate));
        self.age[slot] = self.counter;
        self.counter += 1;
    }

    fn oldest(&self) -> usize {
        (0..MAX_VOICES).min_by_key(|&i| self.age[i]).unwrap_or(0)
    }
}

impl Default for MixerState {
    fn default() -> Self {
        Self::new()
    }
}

/// The body of the audio callback: applies pending commands, then fills interleaved `out`
/// (cleared first) with the mixed voices, clamped to [-1, 1]. Does not allocate.
pub fn process(
    state: &mut MixerState,
    buf: &PlayBuffer,
    cmds: &mut rtrb::Consumer<Command>,
    out: &mut [f32],
    channels: usize,
    device_rate: u32,
) {
    while let Ok(cmd) = cmds.pop() {
        state.apply(cmd, buf, device_rate);
    }
    out.fill(0.0);
    if channels == 0 {
        return;
    }
    for slot in &mut state.voices {
        if let Some(v) = slot
            && !v.render_add(buf, out, channels, device_rate)
        {
            *slot = None;
        }
    }
    for s in out {
        *s = s.clamp(-1.0, 1.0);
    }
}

/// Everything the audio callback owns.
struct Callback {
    state: MixerState,
    buffer: Arc<ArcSwap<PlayBuffer>>,
    ui_rx: rtrb::Consumer<Command>,
    midi_rx: rtrb::Consumer<Command>,
    channels: usize,
    device_rate: u32,
}

impl Callback {
    fn render(&mut self, out: &mut [f32]) {
        let buf = self.buffer.load();
        while let Ok(cmd) = self.midi_rx.pop() {
            self.state.apply(cmd, &buf, self.device_rate);
        }
        process(
            &mut self.state,
            &buf,
            &mut self.ui_rx,
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
    // Declared before the buffers so the stream (and its callback) is dropped first.
    _stream: cpal::Stream,
    buffer: Arc<ArcSwap<PlayBuffer>>,
    /// Buffers swapped out by `set_buffer` that the audio thread may still hold. Each is dropped
    /// here on the UI thread once nothing else holds it, so the audio thread never frees one.
    retired: RefCell<Vec<Arc<PlayBuffer>>>,
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
        let cb = Callback {
            state: MixerState::new(),
            buffer: Arc::clone(&buffer),
            ui_rx,
            midi_rx,
            channels: usize::from(config.channels),
            device_rate,
        };
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
            _stream: stream,
            buffer,
            retired: RefCell::new(Vec::new()),
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

    /// Queues a command for the audio thread. Dropped if the queue is full.
    pub fn send(&mut self, c: Command) {
        let _ = self.ui_tx.push(c);
    }

    /// Releases every note and swaps in an empty buffer, so nothing from the previous file can
    /// keep sounding.
    pub fn silence(&mut self) {
        self.send(Command::AllOff);
        self.set_buffer(empty_buffer(self.device_rate));
    }

    /// The producer end of the MIDI command queue. `Some` on the first call only.
    pub fn midi_sender(&mut self) -> Option<rtrb::Producer<Command>> {
        self.midi_tx.take()
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
    use super::super::voice::RELEASE_MS;
    use super::*;
    use limewave_core::edit::LoopMode;

    const RATE: u32 = 48_000;

    /// A looping stereo buffer of constant 0.25, so every voice keeps sounding.
    fn looping() -> PlayBuffer {
        PlayBuffer {
            audio: vec![vec![0.25; 4000], vec![0.25; 4000]],
            rate: RATE,
            start: 0,
            end: 3999,
            loop_mode: LoopMode::Forward,
            loop_start: 1000,
            loop_end: 2999,
            root: 60,
            cents: 0.0,
        }
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
        let mut out = vec![0.7; frames * 2];
        process(state, b, rx, &mut out, 2, RATE);
        out
    }

    #[test]
    fn engine_mixer_handles_commands_without_device() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        let release_frames = (RELEASE_MS * RATE as f32 / 1000.0).ceil() as usize;

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
        block(&mut state, &b, &mut rx, release_frames);
        let out = block(&mut state, &b, &mut rx, release_frames);
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

        tx.push(Command::AllOff).unwrap();
        block(&mut state, &b, &mut rx, release_frames);
        assert!(active(&state).is_empty());
    }

    #[test]
    fn engine_retrigger_releases_the_sounding_note() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 64);
        // A free slot exists: the old voice fades while the new one starts.
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        block(&mut state, &b, &mut rx, 1);
        assert_eq!(active(&state), [60, 60]);
        let release_frames = (RELEASE_MS * RATE as f32 / 1000.0).ceil() as usize;
        block(&mut state, &b, &mut rx, release_frames);
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
    fn engine_zero_channels_does_not_panic() {
        let b = looping();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(256);
        let mut state = MixerState::new();
        tx.push(Command::NoteOn { note: 60, from: 0 }).unwrap();
        process(&mut state, &b, &mut rx, &mut [], 0, RATE);
    }
}
