//! Audition voice: pitched, interpolated playback of a rendered buffer.

use seamlime_core::edit::{EditState, LoopMode};
use seamlime_core::render::render;

/// Release fade length in milliseconds.
pub const RELEASE_MS: f32 = 10.0;
/// Maximum simultaneous audition voices.
pub const MAX_VOICES: usize = 8;

/// A rendered sample (crossfade applied) plus the markers needed to play it.
#[derive(Clone, Debug)]
pub struct PlayBuffer {
    pub audio: Vec<Vec<f32>>,
    pub rate: u32,
    pub start: usize,
    pub end: usize,
    pub loop_mode: LoopMode,
    pub loop_start: usize,
    pub loop_end: usize,
    pub root: u8,
    pub cents: f32,
}

pub fn play_buffer(doc_audio: &[Vec<f32>], rate: u32, e: &EditState) -> PlayBuffer {
    PlayBuffer {
        audio: render(doc_audio, e),
        rate,
        start: e.start,
        end: e.end,
        loop_mode: e.loop_mode,
        loop_start: e.loop_start,
        loop_end: e.loop_end,
        root: e.root,
        cents: e.cents,
    }
}

/// Frames of source advanced per output frame for `note`. A sample recorded `cents` flat at
/// its root plays faster to reach pitch, hence the subtraction.
pub fn playback_step(note: u8, root: u8, cents: f32, file_rate: u32, device_rate: u32) -> f64 {
    let semitones = f64::from(note) - f64::from(root) - f64::from(cents) / 100.0;
    2f64.powf(semitones / 12.0) * (f64::from(file_rate) / f64::from(device_rate))
}

/// 4-point cubic Hermite interpolation between `x0` and `x1` at fraction `t`.
pub fn hermite(xm1: f32, x0: f32, x1: f32, x2: f32, t: f32) -> f32 {
    let c1 = 0.5 * (x1 - xm1);
    let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
    let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
    ((c3 * t + c2) * t + c1) * t + x0
}

/// A loop that is actually playable: at least two frames inside the buffer.
#[derive(Clone, Copy)]
struct LoopPoints {
    start: usize,
    end: usize,
    ping_pong: bool,
}

impl LoopPoints {
    fn len(&self) -> usize {
        self.end - self.start + 1
    }

    /// Folds an unwrapped index into the loop. Forward loops wrap, ping-pong loops reflect.
    fn map(&self, i: isize) -> usize {
        let d = i - self.start as isize;
        if self.ping_pong {
            let span = (self.end - self.start) as isize;
            let u = d.rem_euclid(2 * span);
            self.start + (if u > span { 2 * span - u } else { u }) as usize
        } else {
            self.start + d.rem_euclid(self.len() as isize) as usize
        }
    }
}

/// The parts of a [`PlayBuffer`] the voice needs, validated against the actual audio length.
struct Region<'a> {
    chans: &'a [Vec<f32>],
    start: usize,
    end: usize,
    looping: Option<LoopPoints>,
}

impl<'a> Region<'a> {
    /// `None` when the buffer has no frames to play.
    fn new(buf: &'a PlayBuffer) -> Option<Region<'a>> {
        let len = buf.audio.iter().map(Vec::len).min()?;
        if len == 0 {
            return None;
        }
        let end = buf.end.min(len - 1);
        let loop_end = buf.loop_end.min(len - 1);
        let looping =
            (buf.loop_mode != LoopMode::Off && buf.loop_start < loop_end).then_some(LoopPoints {
                start: buf.loop_start,
                end: loop_end,
                ping_pong: buf.loop_mode == LoopMode::PingPong,
            });
        Some(Region {
            chans: &buf.audio,
            start: buf.start.min(end),
            end,
            looping,
        })
    }

    /// The source index read for tap `i`, or `None` for silence past the end of a one-shot.
    /// `in_loop` says the voice is inside the loop, so taps outside it fold back in.
    fn tap(&self, i: isize, in_loop: bool) -> Option<usize> {
        if let Some(lp) = self.looping {
            let past_end = i > lp.end as isize;
            let before_start = i < lp.start as isize && in_loop && lp.ping_pong;
            if past_end || before_start {
                return Some(lp.map(i));
            }
        } else if i > self.end as isize {
            return None;
        }
        Some(i.max(self.start as isize) as usize)
    }

    fn sample(&self, ch: &[f32], pos: f64) -> f32 {
        let floor = pos.floor();
        let idx = floor as isize;
        let t = (pos - floor) as f32;
        let in_loop = self.looping.is_some_and(|lp| pos >= lp.start as f64);
        let at = |i: isize| self.tap(i, in_loop).map_or(0.0, |j| ch[j]);
        hermite(at(idx - 1), at(idx), at(idx + 1), at(idx + 2), t)
    }
}

/// One playing note: a fractional read position, direction and release state.
pub struct Voice {
    note: u8,
    pos: f64,
    step: f64,
    forward: bool,
    dead: bool,
    releasing: bool,
    /// Output frames of release fade (set on the first render after `release`) and how many remain.
    release_total: u32,
    release_left: u32,
}

impl Voice {
    pub fn new(note: u8, from: usize, buf: &PlayBuffer, device_rate: u32) -> Voice {
        let region = Region::new(buf);
        let pos = region.as_ref().map_or(0, |r| from.clamp(r.start, r.end));
        Voice {
            note,
            pos: pos as f64,
            step: playback_step(note, buf.root, buf.cents, buf.rate, device_rate),
            forward: true,
            dead: region.is_none(),
            releasing: false,
            release_total: 0,
            release_left: 0,
        }
    }

    /// Starts the linear fade-out; it takes effect on the next `render_add`.
    pub fn release(&mut self) {
        self.releasing = true;
    }

    pub fn note(&self) -> u8 {
        self.note
    }

    /// Mixes this voice into interleaved `out`. Returns whether the voice is still alive.
    /// Does not allocate.
    pub fn render_add(
        &mut self,
        buf: &PlayBuffer,
        out: &mut [f32],
        out_channels: usize,
        device_rate: u32,
    ) -> bool {
        if self.dead {
            return false;
        }
        let Some(region) = Region::new(buf) else {
            self.dead = true;
            return false;
        };
        if out_channels == 0 {
            return true;
        }
        self.step = playback_step(self.note, buf.root, buf.cents, buf.rate, device_rate);
        if self.releasing && self.release_total == 0 {
            let frames = (RELEASE_MS * device_rate as f32 / 1000.0).ceil().max(1.0) as u32;
            self.release_total = frames;
            self.release_left = frames;
        }
        match region.looping {
            // The loop moved under a live voice: restart from its beginning.
            Some(lp) if self.pos > (lp.end + 1) as f64 => {
                self.pos = lp.start as f64;
                self.forward = true;
            }
            Some(lp) if !lp.ping_pong => self.forward = true,
            Some(_) => {}
            None => {
                self.forward = true;
                if self.pos > region.end as f64 {
                    self.dead = true;
                    return false;
                }
            }
        }
        let stereo = region.chans.len() > 1;
        for frame in out.chunks_exact_mut(out_channels) {
            let gain = if self.releasing {
                self.release_left as f32 / self.release_total as f32
            } else {
                1.0
            };
            let l = region.sample(&region.chans[0], self.pos);
            let r = if stereo {
                region.sample(&region.chans[1], self.pos)
            } else {
                l
            };
            if out_channels == 1 {
                frame[0] += if stereo { 0.5 * (l + r) } else { l } * gain;
            } else {
                frame[0] += l * gain;
                frame[1] += r * gain;
                if !stereo {
                    for s in &mut frame[2..] {
                        *s += l * gain;
                    }
                }
            }
            if self.releasing {
                self.release_left -= 1;
                if self.release_left == 0 {
                    self.dead = true;
                    return false;
                }
            }
            if !self.advance(&region) {
                self.dead = true;
                return false;
            }
        }
        true
    }

    /// Moves one output frame along; `false` when a one-shot has run past its end.
    fn advance(&mut self, region: &Region) -> bool {
        if self.forward {
            self.pos += self.step;
        } else {
            self.pos -= self.step;
        }
        match region.looping {
            None => self.pos <= region.end as f64,
            Some(lp) if !lp.ping_pong => {
                if self.pos >= (lp.end + 1) as f64 {
                    self.pos = lp.start as f64 + (self.pos - lp.start as f64) % lp.len() as f64;
                }
                true
            }
            Some(lp) => {
                let (ls, le) = (lp.start as f64, lp.end as f64);
                if self.pos > le || (!self.forward && self.pos < ls) {
                    // Unfold to a coordinate that always advances, then fold back into the loop.
                    let span = le - ls;
                    let d = self.pos - ls;
                    let x = if self.forward { d } else { 2.0 * span - d };
                    let u = x.rem_euclid(2.0 * span);
                    if u > span {
                        self.pos = ls + 2.0 * span - u;
                        self.forward = false;
                    } else {
                        self.pos = ls + u;
                        self.forward = true;
                    }
                }
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn buf(audio: Vec<Vec<f32>>, mode: LoopMode, loop_start: usize, loop_end: usize) -> PlayBuffer {
        let frames = audio.first().map_or(0, Vec::len);
        PlayBuffer {
            audio,
            rate: RATE,
            start: 0,
            end: frames.saturating_sub(1),
            loop_mode: mode,
            loop_start,
            loop_end,
            root: 60,
            cents: 0.0,
        }
    }

    /// A mono ramp 1.0, 2.0, 3.0 ... so every frame is distinguishable.
    fn ramp(frames: usize) -> Vec<Vec<f32>> {
        vec![(0..frames).map(|i| 1.0 + i as f32).collect()]
    }

    fn render_frames(v: &mut Voice, b: &PlayBuffer, frames: usize) -> (Vec<f32>, bool) {
        let mut out = vec![0.0; frames];
        let alive = v.render_add(b, &mut out, 1, RATE);
        (out, alive)
    }

    #[test]
    fn step_unity_same_rate() {
        assert_eq!(playback_step(60, 60, 0.0, 48000, 48000), 1.0);
    }

    #[test]
    fn step_octave_up() {
        assert_eq!(playback_step(72, 60, 0.0, 48000, 48000), 2.0);
    }

    #[test]
    fn step_accounts_for_rate_mismatch() {
        assert_eq!(playback_step(60, 60, 0.0, 44100, 48000), 0.91875);
    }

    #[test]
    fn step_cents() {
        let r = 44100;
        let want = 2f64.powf(12.0 / 1200.0);
        assert!((playback_step(60, 60, -12.0, r, r) - want).abs() < 1e-9);
    }

    #[test]
    fn hermite_at_zero_returns_x0() {
        assert_eq!(hermite(0.3, -0.7, 0.2, 0.9, 0.0), -0.7);
    }

    #[test]
    fn hermite_at_one_returns_x1() {
        assert!((hermite(0.3, -0.7, 0.2, 0.9, 1.0) - 0.2).abs() < 1e-6);
    }

    #[test]
    fn forward_loop_stays_in_loop() {
        let b = buf(ramp(2000), LoopMode::Forward, 1000, 1099);
        let mut v = Voice::new(60, 0, &b, RATE);
        let (_, alive) = render_frames(&mut v, &b, 1000 + 1000);
        assert!(alive);
        assert!(v.pos >= 1000.0 && v.pos < 1100.0, "pos {}", v.pos);
    }

    #[test]
    fn exact_period_loop_is_click_free() {
        let sine: Vec<f32> = (0..2000)
            .map(|i| (2.0 * std::f64::consts::PI * 480.0 * i as f64 / 48_000.0).sin() as f32)
            .collect();
        let natural = sine
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        let b = buf(vec![sine], LoopMode::Forward, 1000, 1099);
        let mut v = Voice::new(60, 1000, &b, RATE);
        let (out, alive) = render_frames(&mut v, &b, 500);
        assert!(alive);
        let worst = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= natural * 1.05, "worst {worst} natural {natural}");
    }

    #[test]
    fn one_shot_ends() {
        let b = buf(ramp(100), LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE);
        let (out, alive) = render_frames(&mut v, &b, 99);
        assert!(alive);
        assert_eq!(out[0], 1.0);
        assert_eq!(out[98], 99.0);
        let (_, alive) = render_frames(&mut v, &b, 5);
        assert!(!alive);
        // Once dead, nothing more is written.
        let (out, alive) = render_frames(&mut v, &b, 5);
        assert!(!alive);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn pingpong_reverses() {
        let b = buf(ramp(2000), LoopMode::PingPong, 1000, 1099);
        let mut v = Voice::new(60, 1090, &b, RATE);
        let (out, _) = render_frames(&mut v, &b, 30);
        // Frames 1090..=1099 forward, then back down.
        assert_eq!(out[9], 1100.0);
        assert!(out[10] < out[9] && out[11] < out[10], "{:?}", &out[8..14]);
        assert!(v.pos < 1099.0);
        // Runs to the bottom and reflects forward again without leaving the loop.
        let mut v = Voice::new(60, 1010, &b, RATE);
        let (out, _) = render_frames(&mut v, &b, 40);
        assert!(out.iter().all(|s| *s >= 1001.0 && *s <= 1100.0));
        assert!(out[39] > out[30], "forward again after the bottom");
    }

    #[test]
    fn release_fades_to_silence() {
        let b = buf(vec![vec![1.0; 2000]], LoopMode::Forward, 100, 1900);
        let mut v = Voice::new(60, 0, &b, RATE);
        render_frames(&mut v, &b, 50);
        v.release();
        let n = (RELEASE_MS / 1000.0 * RATE as f32).ceil() as usize + 1;
        let (out, alive) = render_frames(&mut v, &b, n);
        assert!(!alive);
        assert!(out[0] > 0.9, "starts near full gain, got {}", out[0]);
        assert!(out.windows(2).all(|w| w[1] <= w[0]));
        assert!(out[n - 2].abs() < 0.01);
        assert_eq!(out[n - 1], 0.0);
    }

    #[test]
    fn loop_change_wraps_voice() {
        let b = buf(ramp(1000), LoopMode::Forward, 100, 800);
        let mut v = Voice::new(60, 500, &b, RATE);
        render_frames(&mut v, &b, 10);
        let mut changed = b.clone();
        changed.loop_start = 200;
        changed.loop_end = 300;
        let (_, alive) = render_frames(&mut v, &changed, 10);
        assert!(alive);
        assert!(v.pos >= 200.0 && v.pos < 301.0, "pos {}", v.pos);
    }

    #[test]
    fn zero_frame_buffer_is_silent() {
        let b = buf(vec![Vec::new()], LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE);
        let mut out = vec![0.0; 8];
        assert!(!v.render_add(&b, &mut out, 2, RATE));
        assert!(out.iter().all(|s| *s == 0.0));
        let b = buf(Vec::new(), LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE);
        assert!(!v.render_add(&b, &mut out, 2, RATE));
    }

    #[test]
    fn channel_mapping() {
        let stereo = vec![vec![0.5; 50], vec![-0.5; 50]];
        let b = buf(stereo, LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE);
        let mut out = vec![0.0; 8]; // 4 frames x 2 channels
        v.render_add(&b, &mut out, 2, RATE);
        assert_eq!(&out[..4], &[0.5, -0.5, 0.5, -0.5]);
        let mut v = Voice::new(60, 0, &b, RATE);
        let mut out = vec![0.0; 6]; // 2 frames x 3 channels
        v.render_add(&b, &mut out, 3, RATE);
        assert_eq!(&out[..3], &[0.5, -0.5, 0.0]);
        let mut v = Voice::new(60, 0, &b, RATE);
        let mut out = vec![0.0; 2]; // mono output averages
        v.render_add(&b, &mut out, 1, RATE);
        assert_eq!(out, [0.0, 0.0]);

        let mono = buf(vec![vec![0.25; 50]], LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &mono, RATE);
        let mut out = vec![0.0; 6];
        v.render_add(&mono, &mut out, 3, RATE);
        assert_eq!(&out[..3], &[0.25, 0.25, 0.25]);
    }

    #[test]
    fn mono_output_averages_stereo() {
        let stereo = vec![vec![0.75; 50], vec![0.25; 50]];
        let b = buf(stereo, LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE);
        let mut out = vec![0.0; 2];
        v.render_add(&b, &mut out, 1, RATE);
        assert_eq!(out, [0.5, 0.5]);
    }

    #[test]
    fn render_adds_into_existing_output() {
        let b = buf(vec![vec![0.25; 50]], LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE);
        let mut out = vec![1.0; 4];
        v.render_add(&b, &mut out, 1, RATE);
        assert_eq!(out, [1.25; 4]);
    }

    #[test]
    fn start_is_clamped_into_trim_range() {
        let mut b = buf(ramp(100), LoopMode::Off, 0, 0);
        b.start = 10;
        b.end = 20;
        let mut v = Voice::new(60, 5, &b, RATE);
        let (out, _) = render_frames(&mut v, &b, 1);
        assert_eq!(out[0], 11.0);
        let mut v = Voice::new(60, 50, &b, RATE);
        let (out, _) = render_frames(&mut v, &b, 1);
        assert_eq!(out[0], 21.0);
    }

    #[test]
    fn extreme_pitch_and_tiny_loops_never_panic() {
        let mut seed = 12345u64;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for _ in 0..300 {
            let frames = 1 + next(40) as usize;
            let audio = vec![vec![0.5; frames], vec![-0.5; frames]];
            let mode = [LoopMode::Off, LoopMode::Forward, LoopMode::PingPong][next(3) as usize];
            let mut b = buf(audio, mode, next(45) as usize, next(45) as usize);
            b.start = next(45) as usize;
            b.end = next(45) as usize;
            let mut v = Voice::new(next(128) as u8, next(60) as usize, &b, RATE);
            for _ in 0..4 {
                let mut out = vec![0.0; 2 * 64];
                v.render_add(&b, &mut out, 2, RATE);
                assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
            }
        }
    }

    #[test]
    fn pingpong_reflects_with_octave_step() {
        let b = buf(ramp(2000), LoopMode::PingPong, 1000, 1099);
        let mut v = Voice::new(72, 1000, &b, RATE);
        render_frames(&mut v, &b, 1000);
        assert!(v.pos >= 1000.0 && v.pos <= 1099.0, "pos {}", v.pos);
    }

    #[test]
    fn play_buffer_copies_edit_state() {
        let audio = vec![vec![0.0; 100]];
        let mut e = EditState::default_for(100);
        e.root = 64;
        e.cents = 7.0;
        let p = play_buffer(&audio, 44100, &e);
        assert_eq!((p.rate, p.root, p.cents), (44100, 64, 7.0));
        assert_eq!((p.start, p.end), (e.start, e.end));
        assert_eq!(p.audio[0].len(), 100);
    }
}
