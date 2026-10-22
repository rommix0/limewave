//! Recording from an input device, without the device: the audio callback's [`Feed`] copies
//! samples into a lock-free ring, and the [`Recorder`]'s capture thread drains it into the take.
//!
//! The callback side never allocates, locks or blocks: it touches only atomics and the ring.
//! The capture thread, not the UI, drains the ring, so a minimized window never loses audio.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use InputChoice::{Mono, Pair};

/// Seconds of audio the ring holds.
const RING_SECONDS: usize = 2;
/// The ring never holds fewer frames than this, so a device block fits even at a low rate.
const MIN_RING_FRAMES: usize = 1024;
/// The longest take, in seconds (10 minutes).
const CAP_SECONDS: usize = 600;
/// How often the capture thread drains the ring.
const DRAIN_EVERY: Duration = Duration::from_millis(5);
/// The clip light's level: -0.1 dBFS, `10^(-0.1/20)`.
const CLIP_LEVEL: f32 = 0.988_553_1;

/// Which inputs of a device to record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputChoice {
    /// One input, by index.
    Mono(usize),
    /// An adjacent pair, by the index of its first input.
    Pair(usize),
}

impl InputChoice {
    /// Channels in the take: 1 or 2.
    pub fn channels(self) -> usize {
        match self {
            Mono(_) => 1,
            Pair(_) => 2,
        }
    }

    fn first(self) -> usize {
        match self {
            Mono(i) | Pair(i) => i,
        }
    }

    /// "Mono" on a 1-channel device, else "Input N" or "Inputs N+M", counting from 1.
    pub fn label(self, device_channels: usize) -> String {
        match self {
            Mono(_) if device_channels <= 1 => "Mono".into(),
            Mono(i) => format!("Input {}", i + 1),
            Pair(i) => format!("Inputs {}+{}", i + 1, i + 2),
        }
    }
}

/// Every single input, then every adjacent pair from the first input; `[Mono(0)]` on a
/// 1-channel device.
pub fn input_choices(device_channels: usize) -> Vec<InputChoice> {
    if device_channels <= 1 {
        return vec![Mono(0)];
    }
    (0..device_channels)
        .map(Mono)
        .chain((0..device_channels - 1).step_by(2).map(Pair))
        .collect()
}

/// The first pair when the device has two or more inputs, else its only input.
pub fn default_choice(device_channels: usize) -> InputChoice {
    if device_channels >= 2 {
        Pair(0)
    } else {
        Mono(0)
    }
}

/// Why a take ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Stop was pressed.
    User,
    /// The take reached 10 minutes.
    Cap,
    /// The ring filled up before the capture thread drained it.
    Overflow,
    /// The device failed; the text is the device's error.
    Device(String),
}

impl StopReason {
    /// The notice for a take stopped at the cap or by an overflow. A device error is shown
    /// with its own text.
    pub fn message(&self) -> Option<&'static str> {
        match self {
            StopReason::Cap => Some("Recording stopped at the 10-minute limit"),
            StopReason::Overflow => {
                Some("Recording stopped: audio was lost while the program was busy")
            }
            StopReason::User | StopReason::Device(_) => None,
        }
    }
}

/// A finished take: one `Vec` per chosen channel, at the device's rate.
#[derive(Clone, Debug, PartialEq)]
pub struct Take {
    pub audio: Vec<Vec<f32>>,
    pub rate: u32,
    pub stopped: StopReason,
}

impl Take {
    /// Length in frames.
    pub fn frames(&self) -> usize {
        self.audio.first().map_or(0, Vec::len)
    }
}

/// State shared by the callback, the error callback, the capture thread and the UI.
struct Shared {
    /// Whether `Feed` pushes into the ring.
    recording: AtomicBool,
    /// Set by `Feed` when a block did not fit; it pushes nothing more until the next start.
    overflow: AtomicBool,
    /// Tells the capture thread to exit.
    quit: AtomicBool,
    /// Frames in the take so far, for the UI.
    frames: AtomicUsize,
    /// Per chosen channel: the highest peak since the meter was last read, as `f32` bits.
    /// Peaks are never negative, so their bits order like the values and `fetch_max` works.
    peaks: Box<[AtomicU32]>,
    /// Per chosen channel: the clip light.
    clips: Box<[AtomicBool]>,
    /// The first device error, written only from the stream's error callback.
    error: Mutex<Option<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The audio-callback side: copies the chosen channels into the ring and updates the meter.
/// It never allocates, locks or blocks.
pub struct Feed {
    ring: rtrb::Producer<f32>,
    shared: Arc<Shared>,
    device_channels: usize,
    first: usize,
    channels: usize,
}

impl Feed {
    /// Takes a block of device-interleaved frames. Always updates the meter and the clip
    /// lights; while recording, pushes the chosen channels, interleaved. A block that does not
    /// fit whole sets the overflow flag and is not pushed.
    pub fn push(&mut self, interleaved: &[f32]) {
        let (dc, first, last) = (self.device_channels, self.first, self.first + self.channels);
        let mut peaks = [0.0f32; 2];
        for frame in interleaved.chunks_exact(dc) {
            for (peak, sample) in peaks.iter_mut().zip(&frame[first..last]) {
                // `>` skips NaN, so a bad sample never sticks in the meter.
                if sample.abs() > *peak {
                    *peak = sample.abs();
                }
            }
        }
        let shared = &*self.shared;
        for ((slot, clip), peak) in shared.peaks.iter().zip(&*shared.clips).zip(peaks) {
            slot.fetch_max(peak.to_bits(), Ordering::Relaxed);
            if peak >= CLIP_LEVEL {
                clip.store(true, Ordering::Relaxed);
            }
        }

        if !shared.recording.load(Ordering::Acquire) || shared.overflow.load(Ordering::Relaxed) {
            return;
        }
        let samples = interleaved.len() / dc * self.channels;
        match self.ring.write_chunk_uninit(samples) {
            Ok(chunk) => {
                let chosen = interleaved
                    .chunks_exact(dc)
                    .flat_map(|frame| frame[first..last].iter().copied());
                chunk.fill_from_iter(chosen);
            }
            // Release: the capture thread sees every block pushed before the flag.
            Err(_) => shared.overflow.store(true, Ordering::Release),
        }
    }

    /// A handle for the stream's error callback.
    pub fn errors(&self) -> InputErrors {
        InputErrors(Arc::clone(&self.shared))
    }
}

/// The stream's error callback reports through this; the capture thread stops the take.
#[derive(Clone)]
pub struct InputErrors(Arc<Shared>);

impl InputErrors {
    /// Records a device error. The first one is kept until a take acts on it.
    pub fn report(&self, message: String) {
        lock(&self.0.error).get_or_insert(message);
    }
}

/// The capture thread's side, also locked by `Recorder` to start and stop takes.
struct Capture {
    ring: rtrb::Consumer<f32>,
    /// The take so far, one `Vec` per chosen channel.
    audio: Vec<Vec<f32>>,
    rate: u32,
    cap: usize,
    /// Whether a take is in progress.
    active: bool,
    /// A take the thread stopped by itself, until `poll` takes it.
    finished: Option<Take>,
    #[cfg(test)]
    paused: bool,
}

impl Capture {
    fn frames(&self) -> usize {
        self.audio[0].len()
    }

    /// Moves what the ring holds into the take, cut at the cap. Returns whether the cap is
    /// reached.
    fn drain(&mut self, shared: &Shared) -> bool {
        let n = self.audio.len();
        let room = (self.cap - self.frames()) * n;
        if let Ok(chunk) = self.ring.read_chunk(self.ring.slots()) {
            let (a, b) = chunk.as_slices();
            // Sample by sample: the ring may wrap in the middle of a frame.
            for (i, &sample) in a.iter().chain(b).take(room).enumerate() {
                self.audio[i % n].push(sample);
            }
            chunk.commit_all();
        }
        shared.frames.store(self.frames(), Ordering::Relaxed);
        self.frames() >= self.cap
    }

    /// One pass of the capture thread: drains, and stops the take on the cap, an overflow or
    /// a device error.
    fn tick(&mut self, shared: &Shared) {
        #[cfg(test)]
        if self.paused {
            return;
        }
        if !self.active {
            return;
        }
        // Flags before draining, so the drain takes every block pushed before them.
        let overflow = shared.overflow.load(Ordering::Acquire);
        let failed = lock(&shared.error).is_some();
        let stopped = if self.drain(shared) {
            StopReason::Cap
        } else if overflow {
            StopReason::Overflow
        } else if failed {
            StopReason::Device(lock(&shared.error).take().unwrap_or_default())
        } else {
            return;
        };
        let take = self.finish(shared, stopped);
        self.finished = Some(take);
    }

    fn finish(&mut self, shared: &Shared, stopped: StopReason) -> Take {
        shared.recording.store(false, Ordering::Release);
        self.active = false;
        Take {
            audio: self.audio.iter_mut().map(std::mem::take).collect(),
            rate: self.rate,
            stopped,
        }
    }
}

/// The app side of recording: starts and stops takes and reads the meter. Its capture thread
/// drains the ring every ~5 ms while recording and exits when the `Recorder` drops.
pub struct Recorder {
    shared: Arc<Shared>,
    capture: Arc<Mutex<Capture>>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// A recorder for `choice` on a device of `device_channels` at `rate`, and the [`Feed`] for
    /// its input stream. The ring holds 2 seconds of the chosen channels.
    pub fn new(rate: u32, device_channels: usize, choice: InputChoice) -> (Recorder, Feed) {
        let channels = choice.channels();
        assert!(
            choice.first() + channels <= device_channels,
            "{choice:?} is not an input of a {device_channels}-channel device"
        );
        let ring_frames = (RING_SECONDS * rate as usize).max(MIN_RING_FRAMES);
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_frames * channels);
        let shared = Arc::new(Shared {
            recording: AtomicBool::new(false),
            overflow: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            frames: AtomicUsize::new(0),
            peaks: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            clips: (0..channels).map(|_| AtomicBool::new(false)).collect(),
            error: Mutex::new(None),
        });
        let capture = Arc::new(Mutex::new(Capture {
            ring: consumer,
            audio: vec![Vec::new(); channels],
            rate,
            cap: CAP_SECONDS * rate as usize,
            active: false,
            finished: None,
            #[cfg(test)]
            paused: false,
        }));
        let thread = {
            let (shared, capture) = (Arc::clone(&shared), Arc::clone(&capture));
            thread::spawn(move || {
                while !shared.quit.load(Ordering::Acquire) {
                    lock(&capture).tick(&shared);
                    thread::park_timeout(DRAIN_EVERY);
                }
            })
        };
        let feed = Feed {
            ring: producer,
            shared: Arc::clone(&shared),
            device_channels,
            first: choice.first(),
            channels,
        };
        let recorder = Recorder {
            shared,
            capture,
            thread: Some(thread),
        };
        (recorder, feed)
    }

    /// Starts a new take: clears the take and the clip lights, then records. Audio fed before
    /// this is dropped.
    pub fn start(&mut self) {
        let mut capture = lock(&self.capture);
        let stale = capture.ring.slots();
        if let Ok(chunk) = capture.ring.read_chunk(stale) {
            chunk.commit_all();
        }
        capture.audio.iter_mut().for_each(Vec::clear);
        capture.active = true;
        self.shared.frames.store(0, Ordering::Relaxed);
        self.shared.overflow.store(false, Ordering::Relaxed);
        for clip in &*self.shared.clips {
            clip.store(false, Ordering::Relaxed);
        }
        self.shared.recording.store(true, Ordering::Release);
    }

    /// Stops the take in progress and returns it, or `None` when it has no frames or none is
    /// in progress. A take the thread already stopped by itself waits for [`Recorder::poll`].
    pub fn stop(&mut self) -> Option<Take> {
        self.shared.recording.store(false, Ordering::Release);
        let mut capture = lock(&self.capture);
        if !capture.active {
            return None;
        }
        let stopped = if capture.drain(&self.shared) {
            StopReason::Cap
        } else {
            StopReason::User
        };
        let take = capture.finish(&self.shared, stopped);
        (take.frames() > 0).then_some(take)
    }

    /// A take the capture thread stopped by itself (cap, overflow or device error), once. It
    /// may have no frames when the device failed before any audio arrived; the reason still
    /// needs showing.
    pub fn poll(&mut self) -> Option<Take> {
        lock(&self.capture).finished.take()
    }

    /// Whether a take is being recorded.
    pub fn recording(&self) -> bool {
        self.shared.recording.load(Ordering::Acquire)
    }

    /// Frames captured so far in the current or last take.
    pub fn recorded_frames(&self) -> usize {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Per chosen channel: the linear peak since the last call, and the clip light.
    pub fn meter(&self) -> Vec<(f32, bool)> {
        let shared = &*self.shared;
        shared
            .peaks
            .iter()
            .zip(&*shared.clips)
            .map(|(peak, clip)| {
                let peak = f32::from_bits(peak.swap(0, Ordering::Relaxed));
                (peak, clip.load(Ordering::Relaxed))
            })
            .collect()
    }

    /// Turns off the clip light of chosen channel `channel`.
    pub fn clear_clip(&self, channel: usize) {
        if let Some(clip) = self.shared.clips.get(channel) {
            clip.store(false, Ordering::Relaxed);
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
impl Recorder {
    /// Stops (or resumes) the capture thread's draining. Takes effect before it returns.
    pub(crate) fn pause_drain(&self, paused: bool) {
        lock(&self.capture).paused = paused;
    }
}

#[cfg(test)]
impl Feed {
    /// Pushes `interleaved` in 256-frame blocks, each once the ring has room for it while
    /// recording, as a device would at its own pace. Panics after 5 s without room.
    pub(crate) fn push_waiting(&mut self, interleaved: &[f32]) {
        use std::time::Instant;
        let deadline = Instant::now() + Duration::from_secs(5);
        for block in interleaved.chunks(256 * self.device_channels) {
            let need = block.len() / self.device_channels * self.channels;
            while self.shared.recording.load(Ordering::Acquire) && self.ring.slots() < need {
                assert!(Instant::now() < deadline, "no room in the ring within 5 s");
                thread::sleep(Duration::from_millis(1));
            }
            self.push(block);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use super::InputChoice::{Mono, Pair};
    use super::*;

    const BLOCK: usize = 256;

    /// Two device channels: left is the frame index, right its negative.
    fn ramp(frames: std::ops::Range<usize>) -> Vec<f32> {
        frames.flat_map(|i| [i as f32, -(i as f32)]).collect()
    }

    fn index(frames: std::ops::Range<usize>) -> Vec<f32> {
        frames.map(|i| i as f32).collect()
    }

    /// Pushes `data` in 256-frame blocks without waiting.
    fn push_blocks(feed: &mut Feed, data: &[f32], device_channels: usize) {
        for block in data.chunks(BLOCK * device_channels) {
            feed.push(block);
        }
    }

    /// Polls until the capture thread stops the take by itself, for at most 5 s.
    fn wait_poll(rec: &mut Recorder) -> Take {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(take) = rec.poll() {
                return take;
            }
            assert!(Instant::now() < deadline, "no take within 5 s");
            sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn input_choices_cover_single_inputs_and_pairs() {
        assert_eq!(input_choices(1), vec![Mono(0)]);
        assert_eq!(Mono(0).label(1), "Mono");
        assert_eq!(
            input_choices(4),
            vec![Mono(0), Mono(1), Mono(2), Mono(3), Pair(0), Pair(2)]
        );
        let labels: Vec<String> = input_choices(4).iter().map(|c| c.label(4)).collect();
        assert_eq!(
            labels,
            [
                "Input 1",
                "Input 2",
                "Input 3",
                "Input 4",
                "Inputs 1+2",
                "Inputs 3+4"
            ]
        );
        assert_eq!(input_choices(3), vec![Mono(0), Mono(1), Mono(2), Pair(0)]);
        assert_eq!(Mono(1).channels(), 1);
        assert_eq!(Pair(2).channels(), 2);
        assert_eq!(default_choice(1), Mono(0));
        assert_eq!(default_choice(2), Pair(0));
        assert_eq!(default_choice(8), Pair(0));
    }

    #[test]
    fn a_take_holds_exactly_the_chosen_channels() {
        let (mut rec, mut feed) = Recorder::new(48_000, 2, Pair(0));
        rec.start();
        assert!(rec.recording());
        push_blocks(&mut feed, &ramp(0..1000), 2);
        let take = rec.stop().expect("a take");
        assert!(!rec.recording());
        assert_eq!(take.rate, 48_000);
        assert_eq!(take.stopped, StopReason::User);
        let right: Vec<f32> = (0..1000).map(|i| -(i as f32)).collect();
        assert_eq!(take.audio, vec![index(0..1000), right.clone()]);

        let (mut rec, mut feed) = Recorder::new(48_000, 2, Mono(1));
        rec.start();
        push_blocks(&mut feed, &ramp(0..1000), 2);
        let take = rec.stop().expect("a take");
        assert_eq!(take.audio, vec![right]);
    }

    #[test]
    fn frames_before_start_are_not_recorded_but_meter() {
        let (mut rec, mut feed) = Recorder::new(48_000, 2, Pair(0));
        let data: Vec<f32> = (0..1000).flat_map(|_| [0.25, -0.5]).collect();
        push_blocks(&mut feed, &data, 2);
        assert_eq!(rec.meter(), vec![(0.25, false), (0.5, false)]);
        rec.start();
        assert_eq!(rec.recorded_frames(), 0);
        assert!(rec.stop().is_none());
    }

    #[test]
    fn the_cap_stops_at_exactly_ten_minutes() {
        let (mut rec, mut feed) = Recorder::new(100, 1, Mono(0));
        rec.start();
        feed.push_waiting(&index(0..600 * 100 + 500));
        let take = wait_poll(&mut rec);
        assert_eq!(take.stopped, StopReason::Cap);
        assert_eq!(
            take.stopped.message(),
            Some("Recording stopped at the 10-minute limit")
        );
        assert_eq!(take.audio, vec![index(0..60_000)]);
        assert!(!rec.recording());
        assert!(rec.poll().is_none(), "a take is returned once");
    }

    #[test]
    fn overflow_stops_and_keeps_the_take() {
        let (mut rec, mut feed) = Recorder::new(48_000, 2, Pair(0));
        rec.pause_drain(true);
        rec.start();
        // 400 blocks of 256 frames is past 2 s; the 2 s ring takes 375 whole blocks.
        push_blocks(&mut feed, &ramp(0..400 * BLOCK), 2);
        rec.pause_drain(false);
        let take = wait_poll(&mut rec);
        assert_eq!(take.stopped, StopReason::Overflow);
        assert_eq!(
            take.stopped.message(),
            Some("Recording stopped: audio was lost while the program was busy")
        );
        assert_eq!(take.audio.len(), 2);
        assert_eq!(take.audio[0], index(0..96_000));
        assert!(!rec.recording());
    }

    #[test]
    fn a_device_error_keeps_what_was_captured() {
        let (mut rec, mut feed) = Recorder::new(48_000, 2, Pair(0));
        rec.start();
        push_blocks(&mut feed, &ramp(0..100), 2);
        feed.errors().report("gone".into());
        let take = wait_poll(&mut rec);
        assert_eq!(take.stopped, StopReason::Device("gone".into()));
        assert_eq!(take.stopped.message(), None);
        assert_eq!(take.audio[0], index(0..100));
        assert!(!rec.recording());
    }

    #[test]
    fn capture_thread_drains_without_the_ui() {
        let (mut rec, mut feed) = Recorder::new(1000, 1, Mono(0));
        rec.start();
        // 5 s against a 2 s ring: it only fits if the thread drains on its own.
        for block in index(0..5000).chunks(BLOCK) {
            feed.push(block);
            sleep(Duration::from_millis(10));
        }
        let take = rec.stop().expect("a take");
        assert_eq!(take.stopped, StopReason::User, "no overflow");
        assert_eq!(take.audio, vec![index(0..5000)]);
    }

    #[test]
    fn a_zero_frame_take_is_none() {
        let (mut rec, _feed) = Recorder::new(48_000, 2, Pair(0));
        rec.start();
        assert!(rec.stop().is_none());
        assert!(rec.stop().is_none());
        assert!(rec.poll().is_none());
    }

    #[test]
    fn clip_lights_latch_until_cleared_or_restarted() {
        let (mut rec, mut feed) = Recorder::new(48_000, 2, Pair(0));
        let lights = |rec: &Recorder| rec.meter().iter().map(|m| m.1).collect::<Vec<_>>();
        feed.push(&[1.0, 0.0, 0.1, 0.1]);
        assert_eq!(lights(&rec), [true, false]);
        feed.push(&[0.1, 0.1]);
        assert_eq!(lights(&rec), [true, false], "latched");
        rec.clear_clip(0);
        assert_eq!(lights(&rec), [false, false]);
        // The light is at or above -0.1 dBFS.
        assert!((CLIP_LEVEL - 10f32.powf(-0.1 / 20.0)).abs() < 1e-7);
        feed.push(&[0.98850, -0.98856]);
        assert_eq!(lights(&rec), [false, true]);
        rec.start();
        assert_eq!(lights(&rec), [false, false]);
    }
}
