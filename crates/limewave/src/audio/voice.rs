//! Audition voice: pitched, interpolated playback of a rendered buffer.

use std::sync::Arc;

use limewave_core::edit::{EditState, LoopMode};
use limewave_core::render::render;
use limewave_core::sfz::Ampeg;

/// Maximum simultaneous audition voices.
pub const MAX_VOICES: usize = 16;
/// Longest attack, decay or release time in milliseconds.
const MAX_STAGE_MS: f32 = 10_000.0;

/// The audition amplitude envelope, one for the whole session (v0.2 §4.5).
///
/// Every stage is a linear segment counted in output frames: attack rises 0 → 1, decay falls
/// 1 → `sustain`, which then holds, and release falls from whatever level was reached to 0 in
/// `release_ms`. A zero-length stage is skipped. Linear decay and release are the builder's
/// choice under §4.5.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Adsr {
    pub attack_ms: f32,
    pub decay_ms: f32,
    /// Sustain level, 0 to 1.
    pub sustain: f32,
    pub release_ms: f32,
}

impl Adsr {
    /// Attack 2 ms, decay 0 ms, sustain 100 %, release 30 ms.
    pub const DEFAULT: Adsr = Adsr {
        attack_ms: 2.0,
        decay_ms: 0.0,
        sustain: 1.0,
        release_ms: 30.0,
    };

    /// Times limited to 0…10 000 ms and sustain to 0…1; a NaN field takes its default value.
    pub fn clamped(self) -> Adsr {
        let fix = |v: f32, default: f32, max: f32| {
            if v.is_nan() {
                default
            } else {
                v.clamp(0.0, max)
            }
        };
        let d = Adsr::DEFAULT;
        Adsr {
            attack_ms: fix(self.attack_ms, d.attack_ms, MAX_STAGE_MS),
            decay_ms: fix(self.decay_ms, d.decay_ms, MAX_STAGE_MS),
            sustain: fix(self.sustain, d.sustain, 1.0),
            release_ms: fix(self.release_ms, d.release_ms, MAX_STAGE_MS),
        }
    }

    /// The envelope as `"attack,decay,sustain,release"`, for the saved app settings.
    pub fn to_storage_string(&self) -> String {
        format!(
            "{},{},{},{}",
            self.attack_ms, self.decay_ms, self.sustain, self.release_ms
        )
    }

    /// Reads [`Adsr::to_storage_string`] back, clamped. Anything other than exactly four finite
    /// numbers gives `None`.
    pub fn from_storage_string(s: &str) -> Option<Adsr> {
        let mut parts = s.split(',').map(|p| p.trim().parse::<f32>().ok());
        let mut next = || parts.next().flatten().filter(|v| v.is_finite());
        let adsr = Adsr {
            attack_ms: next()?,
            decay_ms: next()?,
            sustain: next()?,
            release_ms: next()?,
        };
        parts.next().is_none().then(|| adsr.clamped())
    }
}

/// An instrument region's `ampeg_*` values as an [`Adsr`]: seconds to milliseconds and the
/// sustain percentage to 0…1, clamped.
pub fn ampeg_to_adsr(a: &Ampeg) -> Adsr {
    Adsr {
        attack_ms: a.attack * 1000.0,
        decay_ms: a.decay * 1000.0,
        sustain: a.sustain / 100.0,
        release_ms: a.release * 1000.0,
    }
    .clamped()
}

/// Output frames an envelope stage of `ms` lasts at `rate`.
fn stage_frames(ms: f32, rate: u32) -> u32 {
    // `as` saturates, so a negative or NaN time is 0 frames.
    (f64::from(ms) * f64::from(rate) / 1000.0).round() as u32
}

/// A rendered sample (crossfade applied) plus the markers needed to play it.
#[derive(Clone, Debug)]
pub struct PlayBuffer {
    /// Shared, so the regions of an instrument can each hold a cheap copy.
    pub audio: Arc<Vec<Vec<f32>>>,
    pub rate: u32,
    pub start: usize,
    pub end: usize,
    pub loop_mode: LoopMode,
    pub loop_start: usize,
    pub loop_end: usize,
    pub root: u8,
    pub cents: f32,
    /// Which session the audio comes from (0 for the empty buffer). Voices only ever play the
    /// source they started on, so a voice released on one file finishes on that file's audio.
    pub source: u64,
}

pub fn play_buffer(doc_audio: &[Vec<f32>], rate: u32, e: &EditState, source: u64) -> PlayBuffer {
    PlayBuffer {
        audio: Arc::new(render(doc_audio, e)),
        rate,
        start: e.start,
        end: e.end,
        loop_mode: e.loop_mode,
        loop_start: e.loop_start,
        loop_end: e.loop_end,
        root: e.root,
        cents: e.cents,
        source,
    }
}

/// How an instrument region's voice ends (spec §6.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Playback {
    /// Released in place: a looped region keeps looping through its release.
    Normal,
    /// Ignores the loop and note-off and plays to End.
    OneShot,
    /// Loops while held, then leaves the loop on release and plays on to End.
    Sustain,
}

/// One region of an instrument, ready to play.
#[derive(Clone, Debug)]
pub struct RegionPlay {
    /// The region's sample with the region's start, end, loop and pitch. Its `source` is the
    /// region's own and stays the same while the region is edited.
    pub buffer: PlayBuffer,
    pub lokey: u8,
    pub hikey: u8,
    pub lovel: u8,
    pub hivel: u8,
    /// Linear gain, from the region's `volume`.
    pub gain: f32,
    /// How much velocity changes the level, 0 to 1 ([`velocity_gain`]).
    pub veltrack: f32,
    pub playback: Playback,
    pub adsr: Adsr,
}

impl RegionPlay {
    /// Whether a note-on with `note` and `velocity` plays this region.
    pub fn matches(&self, note: u8, velocity: u8) -> bool {
        (self.lokey..=self.hikey).contains(&note) && (self.lovel..=self.hivel).contains(&velocity)
    }
}

/// An instrument's regions as the engine plays them. `source` tells one instrument (or a
/// rebuilt one) from another, as [`PlayBuffer::source`] does for files.
#[derive(Clone, Debug)]
pub struct InstrumentPlay {
    pub regions: Vec<RegionPlay>,
    pub source: u64,
}

/// The level a note of `velocity` plays at (spec §6.2): with `veltrack` 1 it is
/// `(velocity/127)²`, with 0 velocity changes nothing, and in between the curve is mixed
/// linearly in dB, which makes it `(velocity/127)^(2 × veltrack)`. A NaN `veltrack` counts
/// as 1.
pub fn velocity_gain(velocity: u8, veltrack: f32) -> f32 {
    let track = if veltrack.is_nan() {
        1.0
    } else {
        veltrack.clamp(0.0, 1.0)
    };
    let curve = (f32::from(velocity.min(127)) / 127.0).powi(2);
    // 0 to a positive power is 0 and anything to the power 0 is 1, so no log of 0 is taken.
    curve.powf(track)
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
    /// Forward, PingPong or Backward; never Off.
    mode: LoopMode,
}

impl LoopPoints {
    fn len(&self) -> usize {
        self.end - self.start + 1
    }

    /// Folds an unwrapped index into the loop: reflecting at both ends when `reflect`, else
    /// wrapping modulo the loop length.
    fn map(&self, i: isize, reflect: bool) -> usize {
        let d = i - self.start as isize;
        if reflect {
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
    /// `None` when the buffer has no frames to play. A play range (`range`) ignores the loop
    /// and the Start and End markers: it can read the whole audio.
    fn new(buf: &'a PlayBuffer, range: bool) -> Option<Region<'a>> {
        let len = buf.audio.iter().map(Vec::len).min()?;
        if len == 0 {
            return None;
        }
        if range {
            return Some(Region {
                chans: &buf.audio,
                start: 0,
                end: len - 1,
                looping: None,
            });
        }
        let end = buf.end.min(len - 1);
        let loop_end = buf.loop_end.min(len - 1);
        let looping =
            (buf.loop_mode != LoopMode::Off && buf.loop_start < loop_end).then_some(LoopPoints {
                start: buf.loop_start,
                end: loop_end,
                mode: buf.loop_mode,
            });
        Some(Region {
            chans: &buf.audio,
            start: buf.start.min(end),
            end,
            looping,
        })
    }

    /// The source index read for tap `i`, or `None` for silence past the end of a one-shot.
    /// `in_loop` says the voice is inside the loop and `forward` which way it is going, so
    /// taps outside the loop fold back in the order the loop plays.
    fn tap(&self, i: isize, in_loop: bool, forward: bool) -> Option<usize> {
        if let Some(lp) = self.looping {
            let past_end = i > lp.end as isize;
            let before_start = i < lp.start as isize && in_loop;
            match lp.mode {
                LoopMode::PingPong if past_end || before_start => return Some(lp.map(i, true)),
                // Backward loops reflect at the end on the way in, then wrap while backward.
                LoopMode::Backward if past_end && forward => return Some(lp.map(i, true)),
                LoopMode::Backward if (past_end || before_start) && !forward => {
                    return Some(lp.map(i, false));
                }
                LoopMode::Forward if past_end => return Some(lp.map(i, false)),
                _ => {}
            }
        } else if i > self.end as isize {
            return None;
        }
        Some(i.max(self.start as isize) as usize)
    }

    fn sample(&self, ch: &[f32], pos: f64, forward: bool) -> f32 {
        let floor = pos.floor();
        let idx = floor as isize;
        let t = (pos - floor) as f32;
        let in_loop = self.looping.is_some_and(|lp| pos >= lp.start as f64);
        let at = |i: isize| self.tap(i, in_loop, forward).map_or(0.0, |j| ch[j]);
        hermite(at(idx - 1), at(idx), at(idx + 1), at(idx + 2), t)
    }
}

/// Where a voice is in its [`Adsr`].
#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    Attack,
    Decay,
    Sustain,
    Release,
}

/// One playing note: a fractional read position, direction and envelope state.
pub struct Voice {
    note: u8,
    /// The [`PlayBuffer::source`] this voice started on; it only plays that audio.
    source: u64,
    pos: f64,
    step: f64,
    forward: bool,
    dead: bool,
    /// Play range: release on reaching this frame, ignoring the loop and End.
    stop_at: Option<usize>,
    adsr: Adsr,
    stage: Stage,
    /// Output frames played in `stage` so far.
    stage_pos: u32,
    /// The level the release started from.
    release_from: f32,
    /// On release, leave the loop: turn forward and play on to End.
    past_loop: bool,
    /// Released with `past_loop`: the loop is ignored from now on.
    leaving: bool,
    /// The release holds `release_from` instead of fading (preview voices).
    hold_release: bool,
    /// The envelope level of the last frame played.
    level: f32,
    /// Scales every frame: an instrument region's level and velocity, else 1.
    gain: f32,
    /// Plays an instrument region: it keeps the region's envelope when the session's changes.
    instrument: bool,
    /// Ignores the loop and [`Voice::release`] and plays to End ([`Playback::OneShot`]).
    one_shot: bool,
}

impl Voice {
    /// A voice for `note` starting at frame `from`, clamped into what it may play. With
    /// `stop_at`, it ignores the loop and End and releases on reaching that frame.
    pub fn new(
        note: u8,
        from: usize,
        buf: &PlayBuffer,
        device_rate: u32,
        adsr: Adsr,
        stop_at: Option<usize>,
    ) -> Voice {
        let region = Region::new(buf, stop_at.is_some());
        let pos = region.as_ref().map_or(0, |r| from.clamp(r.start, r.end));
        Voice {
            note,
            source: buf.source,
            pos: pos as f64,
            step: playback_step(note, buf.root, buf.cents, buf.rate, device_rate),
            forward: true,
            dead: region.is_none(),
            stop_at,
            adsr,
            stage: Stage::Attack,
            stage_pos: 0,
            release_from: 0.0,
            past_loop: false,
            leaving: false,
            hold_release: false,
            level: 0.0,
            gain: 1.0,
            instrument: false,
            one_shot: false,
        }
    }

    /// Makes this an instrument region's voice (spec §6.2): scaled by `gain` (a non-finite
    /// gain is silent), keeping its own envelope, and ending as `playback` says. A
    /// [`Playback::Sustain`] voice leaves the loop when released; a [`Playback::Normal`] one
    /// stays in it; a [`Playback::OneShot`] voice ignores the loop from the start and plays
    /// straight through to End, ignoring note-off.
    pub fn for_region(mut self, gain: f32, playback: Playback) -> Voice {
        self.gain = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
        self.instrument = true;
        self.past_loop = playback == Playback::Sustain;
        if playback == Playback::OneShot {
            self.one_shot = true;
            self.leaving = true;
            self.forward = true;
        }
        self
    }

    /// Sets whether a release leaves the loop and plays on to End (v0.3 §7.3).
    pub fn release_past_loop(mut self, on: bool) -> Voice {
        self.past_loop = on;
        self
    }

    /// Makes this a dialog preview voice (plan ruling R2): it keeps only the attack (no decay,
    /// full sustain), its release always leaves the loop, and the release holds the level
    /// reached instead of fading, so the generated audio is heard as rendered and the voice
    /// ends at End. [`Voice::fade_out`] still fades it. Without `hold_loop` it ignores the
    /// loop from the start, playing straight through to End.
    pub fn preview(mut self, hold_loop: bool) -> Voice {
        self.adsr.decay_ms = 0.0;
        self.adsr.sustain = 1.0;
        self.past_loop = true;
        self.hold_release = true;
        if !hold_loop {
            self.leaving = true;
            self.forward = true;
        }
        self
    }

    /// Starts the release from the current level; it takes effect on the next `render_add`.
    /// With [`Voice::release_past_loop`] set, the voice also turns forward and stops wrapping,
    /// so it plays through the loop end and ends at End like a one-shot. A one-shot region's
    /// voice ignores it; only [`Voice::fade_out`] stops one early.
    pub fn release(&mut self) {
        if self.one_shot {
            return;
        }
        if self.stage != Stage::Release {
            self.stage = Stage::Release;
            self.stage_pos = 0;
            self.release_from = self.level;
            if self.past_loop {
                self.leaving = true;
                self.forward = true;
            }
        }
    }

    /// Changes the release time. A voice already releasing carries on from the level it has
    /// reached, taking the new time to reach 0, so there is no jump. The same time again
    /// changes nothing, so a repeated envelope never holds a release back.
    pub fn set_release_ms(&mut self, ms: f32) {
        if ms == self.adsr.release_ms {
            return;
        }
        self.adsr.release_ms = ms;
        if self.stage == Stage::Release {
            self.stage_pos = 0;
            self.release_from = self.level;
        }
    }

    /// Fades the voice out from its current level over `ms`, whatever stage it is in. Used when
    /// the voice is stolen, so it ends quickly without a jump.
    pub fn fade_out(&mut self, ms: f32) {
        self.hold_release = false;
        self.stage = Stage::Release;
        self.stage_pos = 0;
        self.release_from = self.level;
        self.adsr.release_ms = ms;
    }

    /// Whether the voice is in its release.
    pub fn is_releasing(&self) -> bool {
        self.stage == Stage::Release
    }

    /// Whether this is a preview voice ([`Voice::preview`]) that has not been faded out.
    pub fn is_preview(&self) -> bool {
        self.hold_release
    }

    /// Whether this voice plays a [`Playback::OneShot`] region.
    pub fn is_one_shot(&self) -> bool {
        self.one_shot
    }

    /// Whether this voice plays an instrument region ([`Voice::for_region`]).
    pub fn is_instrument(&self) -> bool {
        self.instrument
    }

    pub fn note(&self) -> u8 {
        self.note
    }

    pub fn source(&self) -> u64 {
        self.source
    }

    /// The envelope level of the last frame played.
    pub fn level(&self) -> f32 {
        self.level
    }

    /// The read position in file frames.
    pub fn position(&self) -> f64 {
        self.pos
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
        let Some(mut region) = Region::new(buf, self.stop_at.is_some()) else {
            self.dead = true;
            return false;
        };
        if self.leaving {
            region.looping = None;
        }
        if out_channels == 0 {
            return true;
        }
        self.step = playback_step(self.note, buf.root, buf.cents, buf.rate, device_rate);
        match region.looping {
            // The loop moved under a live voice: restart from its beginning.
            Some(lp) if self.pos > (lp.end + 1) as f64 => {
                self.pos = lp.start as f64;
                self.forward = true;
            }
            Some(lp) if lp.mode == LoopMode::Forward => self.forward = true,
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
            if self.release_over(device_rate) {
                self.dead = true;
                return false;
            }
            let gain = self.next_level(device_rate) * self.gain;
            let l = region.sample(&region.chans[0], self.pos, self.forward);
            let r = if stereo {
                region.sample(&region.chans[1], self.pos, self.forward)
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
            if !self.advance(&region) {
                self.dead = true;
                return false;
            }
            if self.stop_at.is_some_and(|to| self.pos >= to as f64) {
                self.release();
            }
        }
        // Report the end of a release at once rather than on the next call.
        if self.release_over(device_rate) {
            self.dead = true;
            return false;
        }
        true
    }

    /// Whether the release has played all its frames. A held release lasts until End, unless
    /// it holds silence (released before any level was reached).
    fn release_over(&self, rate: u32) -> bool {
        self.stage == Stage::Release
            && if self.hold_release {
                self.release_from <= 0.0
            } else {
                self.stage_pos >= stage_frames(self.adsr.release_ms, rate)
            }
    }

    /// Moves the envelope on by one output frame and returns that frame's level. Attack and
    /// decay frames end on their target level; release frames start from the level reached.
    fn next_level(&mut self, rate: u32) -> f32 {
        let a = self.adsr;
        loop {
            let (ms, next) = match self.stage {
                Stage::Attack => (a.attack_ms, Stage::Decay),
                Stage::Decay => (a.decay_ms, Stage::Sustain),
                Stage::Sustain => {
                    self.level = a.sustain;
                    return self.level;
                }
                Stage::Release if self.hold_release => {
                    self.level = self.release_from;
                    return self.level;
                }
                Stage::Release => {
                    let len = stage_frames(a.release_ms, rate).max(1);
                    let t = self.stage_pos as f32 / len as f32;
                    self.level = self.release_from * (1.0 - t).max(0.0);
                    self.stage_pos = self.stage_pos.saturating_add(1);
                    return self.level;
                }
            };
            let len = stage_frames(ms, rate);
            if self.stage_pos < len {
                self.stage_pos += 1;
                let t = self.stage_pos as f32 / len as f32;
                self.level = if self.stage == Stage::Attack {
                    t
                } else {
                    1.0 + (a.sustain - 1.0) * t
                };
                return self.level;
            }
            // Finished, or zero-length: move straight on.
            self.stage = next;
            self.stage_pos = 0;
        }
    }

    /// Moves one output frame along; `false` when a one-shot has run past its end.
    fn advance(&mut self, region: &Region) -> bool {
        if self.forward {
            self.pos += self.step;
        } else {
            self.pos -= self.step;
        }
        let Some(lp) = region.looping else {
            return self.pos <= region.end as f64;
        };
        let (ls, le) = (lp.start as f64, lp.end as f64);
        match lp.mode {
            LoopMode::PingPong => {
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
            }
            LoopMode::Backward => {
                // Past the end on the way in: reflect and play backward from then on.
                if self.forward && self.pos > le {
                    self.pos = 2.0 * le - self.pos;
                    self.forward = false;
                }
                // Below the start while backward: jump up by the loop length.
                if !self.forward && self.pos < ls {
                    self.pos = ls + (self.pos - ls).rem_euclid(lp.len() as f64);
                }
            }
            _ => {
                if self.pos >= le + 1.0 {
                    self.pos = ls + (self.pos - ls) % lp.len() as f64;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    #[test]
    fn adsr_storage_string_round_trips() {
        let d = Adsr::DEFAULT;
        assert_eq!(Adsr::from_storage_string(&d.to_storage_string()), Some(d));
        assert_eq!(Adsr::from_storage_string("1,2,3"), None);
        assert_eq!(Adsr::from_storage_string("a,b,c,d"), None);
        assert_eq!(Adsr::from_storage_string("NaN,0,1,30"), None);
        assert_eq!(
            Adsr::from_storage_string("20000,0,2,5"),
            Some(Adsr {
                attack_ms: 10_000.0,
                decay_ms: 0.0,
                sustain: 1.0,
                release_ms: 5.0,
            })
        );
    }

    /// No attack or decay, so the samples come out unchanged until the release.
    const FLAT: Adsr = Adsr {
        attack_ms: 0.0,
        decay_ms: 0.0,
        sustain: 1.0,
        release_ms: 10.0,
    };

    fn buf(audio: Vec<Vec<f32>>, mode: LoopMode, loop_start: usize, loop_end: usize) -> PlayBuffer {
        let frames = audio.first().map_or(0, Vec::len);
        PlayBuffer {
            audio: Arc::new(audio),
            rate: RATE,
            start: 0,
            end: frames.saturating_sub(1),
            loop_mode: mode,
            loop_start,
            loop_end,
            root: 60,
            cents: 0.0,
            source: 1,
        }
    }

    /// A constant 1.0 mono buffer, so the output is the envelope level.
    fn ones(frames: usize, mode: LoopMode, loop_start: usize, loop_end: usize) -> PlayBuffer {
        buf(vec![vec![1.0; frames]], mode, loop_start, loop_end)
    }

    fn adsr(attack_ms: f32, decay_ms: f32, sustain: f32, release_ms: f32) -> Adsr {
        Adsr {
            attack_ms,
            decay_ms,
            sustain,
            release_ms,
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
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
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
        let mut v = Voice::new(60, 1000, &b, RATE, FLAT, None);
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
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
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
        let mut v = Voice::new(60, 1090, &b, RATE, FLAT, None);
        let (out, _) = render_frames(&mut v, &b, 30);
        // Frames 1090..=1099 forward, then back down.
        assert_eq!(out[9], 1100.0);
        assert!(out[10] < out[9] && out[11] < out[10], "{:?}", &out[8..14]);
        assert!(v.pos < 1099.0);
        // Runs to the bottom and reflects forward again without leaving the loop.
        let mut v = Voice::new(60, 1010, &b, RATE, FLAT, None);
        let (out, _) = render_frames(&mut v, &b, 40);
        assert!(out.iter().all(|s| *s >= 1001.0 && *s <= 1100.0));
        assert!(out[39] > out[30], "forward again after the bottom");
    }

    #[test]
    fn release_fades_to_silence() {
        let b = buf(vec![vec![1.0; 2000]], LoopMode::Forward, 100, 1900);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        render_frames(&mut v, &b, 50);
        v.release();
        let n = 480 + 1;
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
        let mut v = Voice::new(60, 500, &b, RATE, FLAT, None);
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
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        let mut out = vec![0.0; 8];
        assert!(!v.render_add(&b, &mut out, 2, RATE));
        assert!(out.iter().all(|s| *s == 0.0));
        let b = buf(Vec::new(), LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        assert!(!v.render_add(&b, &mut out, 2, RATE));
    }

    #[test]
    fn channel_mapping() {
        let stereo = vec![vec![0.5; 50], vec![-0.5; 50]];
        let b = buf(stereo, LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        let mut out = vec![0.0; 8]; // 4 frames x 2 channels
        v.render_add(&b, &mut out, 2, RATE);
        assert_eq!(&out[..4], &[0.5, -0.5, 0.5, -0.5]);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        let mut out = vec![0.0; 6]; // 2 frames x 3 channels
        v.render_add(&b, &mut out, 3, RATE);
        assert_eq!(&out[..3], &[0.5, -0.5, 0.0]);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        let mut out = vec![0.0; 2]; // mono output averages
        v.render_add(&b, &mut out, 1, RATE);
        assert_eq!(out, [0.0, 0.0]);

        let mono = buf(vec![vec![0.25; 50]], LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &mono, RATE, FLAT, None);
        let mut out = vec![0.0; 6];
        v.render_add(&mono, &mut out, 3, RATE);
        assert_eq!(&out[..3], &[0.25, 0.25, 0.25]);
    }

    #[test]
    fn mono_output_averages_stereo() {
        let stereo = vec![vec![0.75; 50], vec![0.25; 50]];
        let b = buf(stereo, LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        let mut out = vec![0.0; 2];
        v.render_add(&b, &mut out, 1, RATE);
        assert_eq!(out, [0.5, 0.5]);
    }

    #[test]
    fn render_adds_into_existing_output() {
        let b = buf(vec![vec![0.25; 50]], LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        let mut out = vec![1.0; 4];
        v.render_add(&b, &mut out, 1, RATE);
        assert_eq!(out, [1.25; 4]);
    }

    #[test]
    fn start_is_clamped_into_trim_range() {
        let mut b = buf(ramp(100), LoopMode::Off, 0, 0);
        b.start = 10;
        b.end = 20;
        let mut v = Voice::new(60, 5, &b, RATE, FLAT, None);
        let (out, _) = render_frames(&mut v, &b, 1);
        assert_eq!(out[0], 11.0);
        let mut v = Voice::new(60, 50, &b, RATE, FLAT, None);
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
            let mode = [
                LoopMode::Off,
                LoopMode::Forward,
                LoopMode::PingPong,
                LoopMode::Backward,
            ][next(4) as usize];
            let mut b = buf(audio, mode, next(45) as usize, next(45) as usize);
            b.start = next(45) as usize;
            b.end = next(45) as usize;
            let stop_at = (next(2) == 0).then(|| next(60) as usize);
            let mut v = Voice::new(
                next(128) as u8,
                next(60) as usize,
                &b,
                RATE,
                Adsr::DEFAULT,
                stop_at,
            );
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
        let mut v = Voice::new(72, 1000, &b, RATE, FLAT, None);
        render_frames(&mut v, &b, 1000);
        assert!(v.pos >= 1000.0 && v.pos <= 1099.0, "pos {}", v.pos);
    }

    #[test]
    fn play_buffer_copies_edit_state() {
        let audio = vec![vec![0.0; 100]];
        let mut e = EditState::default_for(100);
        e.root = 64;
        e.cents = 7.0;
        let p = play_buffer(&audio, 44100, &e, 9);
        assert_eq!((p.rate, p.root, p.cents, p.source), (44100, 64, 7.0, 9));
        assert_eq!((p.start, p.end), (e.start, e.end));
        assert_eq!(p.audio[0].len(), 100);
    }

    #[test]
    fn attack_ramps_linearly() {
        let b = ones(2000, LoopMode::Forward, 100, 1999);
        let mut v = Voice::new(60, 0, &b, RATE, adsr(10.0, 0.0, 1.0, 30.0), None);
        let (out, alive) = render_frames(&mut v, &b, 600);
        assert!(alive);
        let tol = 1.0 / 480.0 + 1e-6;
        assert!((out[240] - 0.5).abs() <= tol, "frame 240 {}", out[240]);
        assert!((out[479] - 1.0).abs() <= tol, "frame 479 {}", out[479]);
        assert!(
            out[..480].windows(2).all(|w| w[1] > w[0]),
            "rises every frame"
        );
        assert!(
            out[480..].iter().all(|s| *s == 1.0),
            "then holds full level"
        );
    }

    #[test]
    fn decay_reaches_sustain_and_holds() {
        let b = ones(4000, LoopMode::Forward, 100, 3999);
        let mut v = Voice::new(60, 0, &b, RATE, adsr(0.0, 10.0, 0.5, 30.0), None);
        let (out, alive) = render_frames(&mut v, &b, 2001);
        assert!(alive);
        assert!(
            out[..480].windows(2).all(|w| w[1] < w[0]),
            "falls during decay"
        );
        assert!(
            out[480..=2000].iter().all(|s| (*s - 0.5).abs() < 1e-6),
            "holds sustain: {:?}",
            &out[478..484]
        );
        assert!((v.level() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn release_from_mid_attack_has_no_jump() {
        let b = ones(4000, LoopMode::Forward, 100, 3999);
        let mut v = Voice::new(60, 0, &b, RATE, adsr(10.0, 0.0, 1.0, 10.0), None);
        let (before, _) = render_frames(&mut v, &b, 240);
        let reached = before[239];
        assert!(
            (reached - 0.5).abs() <= 1.0 / 480.0 + 1e-6,
            "mid attack {reached}"
        );
        v.release();
        let (out, alive) = render_frames(&mut v, &b, 600);
        assert!(!alive, "the voice died");
        assert!(out[0] <= reached, "no jump up: {} after {reached}", out[0]);
        assert!(out.windows(2).all(|w| w[1] <= w[0]), "never rises");
        let silent = out.iter().position(|s| *s == 0.0).unwrap();
        assert!(silent.abs_diff(480) <= 1, "reached 0 after {silent} frames");
        assert!(out[silent..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn zero_attack_starts_at_full_level() {
        let b = ones(2000, LoopMode::Forward, 100, 1999);
        let mut v = Voice::new(60, 0, &b, RATE, adsr(0.0, 0.0, 1.0, 30.0), None);
        let (out, _) = render_frames(&mut v, &b, 4);
        assert_eq!(out, [1.0; 4]);
    }

    #[test]
    fn one_shot_ends_at_the_sample_end_despite_sustain() {
        let b = ones(100, LoopMode::Off, 0, 0);
        let mut v = Voice::new(60, 0, &b, RATE, adsr(0.0, 0.0, 1.0, 30.0), None);
        let (out, alive) = render_frames(&mut v, &b, 200);
        assert!(!alive, "a held one-shot still ends");
        assert!(out[..100].iter().all(|s| *s == 1.0));
        assert!(out[100..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn backward_loop_plays_forward_then_backward_and_wraps() {
        let x = vec![(0..400).map(|i| i as f32).collect::<Vec<f32>>()];
        let b = buf(x, LoopMode::Backward, 100, 199);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, None);
        let mut positions = Vec::new();
        for _ in 0..500 {
            let pos = v.position();
            positions.push(pos);
            let (out, alive) = render_frames(&mut v, &b, 1);
            assert!(alive);
            // Whole-frame positions read the sample itself.
            assert_eq!(f64::from(out[0]), pos);
        }
        let want: Vec<f64> = (0..=199)
            .chain((100..=198).rev())
            .chain((0..).flat_map(|_| (100..=199).rev()))
            .take(500)
            .map(f64::from)
            .collect();
        assert_eq!(positions, want);
    }

    #[test]
    fn backward_loop_taps_follow_the_playing_order() {
        let x: Vec<f32> = (0..400).map(|i| (i as f32 * 0.37).sin()).collect();
        let b = buf(vec![x.clone()], LoopMode::Backward, 100, 199);
        let region = Region::new(&b, false).unwrap();
        // Going backward, the frame after loop_start is loop_end: the taps wrap.
        let got = region.sample(&region.chans[0], 100.5, false);
        let want = hermite(x[199], x[100], x[101], x[102], 0.5);
        assert!((got - want).abs() < 1e-6, "backward: got {got} want {want}");
        let got = region.sample(&region.chans[0], 199.5, false);
        let want = hermite(x[198], x[199], x[100], x[101], 0.5);
        assert!(
            (got - want).abs() < 1e-6,
            "after the jump: got {got} want {want}"
        );
        // On the first, forward pass the voice reflects at loop_end.
        let got = region.sample(&region.chans[0], 198.5, true);
        let want = hermite(x[197], x[198], x[199], x[198], 0.5);
        assert!((got - want).abs() < 1e-6, "forward: got {got} want {want}");
    }

    #[test]
    fn play_range_releases_at_the_selection_end() {
        let b = ones(2000, LoopMode::Forward, 100, 150);
        let mut v = Voice::new(60, 0, &b, RATE, FLAT, Some(199));
        let mut positions = Vec::new();
        let mut levels = Vec::new();
        loop {
            positions.push(v.position());
            let (out, alive) = render_frames(&mut v, &b, 1);
            levels.push(out[0]);
            if !alive || positions.len() > 2000 {
                break;
            }
        }
        assert!(
            positions.windows(2).all(|w| w[1] > w[0]),
            "passes the loop end without wrapping"
        );
        let at_end = positions.iter().position(|p| *p == 199.0).unwrap();
        assert!(
            levels[..at_end].iter().all(|s| *s == 1.0),
            "full level up to 199"
        );
        assert!(levels[at_end + 1] < 1.0, "releasing after 199");
        let tail = positions.len() - at_end;
        assert!(tail <= 480, "died {tail} frames after 199");
        assert!(tail >= 470, "the release ran its course: {tail}");
    }

    /// Ones up to the loop end at 1999 (loop 1000..=1999), then twos to End at 2999.
    fn ones_then_twos() -> PlayBuffer {
        let mut x = vec![1.0; 3000];
        x[2000..].fill(2.0);
        buf(vec![x], LoopMode::Forward, 1000, 1999)
    }

    /// Holds from 1000 for 1500 frames (to frame 1500, wrapped), releases, then renders 2000.
    fn release_into(b: &PlayBuffer, past_loop: bool) -> (Voice, Vec<f32>, bool) {
        let mut v = Voice::new(60, 1000, b, RATE, adsr(0.0, 0.0, 1.0, 10_000.0), None)
            .release_past_loop(past_loop);
        let (held, _) = render_frames(&mut v, b, 1500);
        assert!(held.iter().all(|s| *s == 1.0));
        assert_eq!(v.pos, 1500.0);
        v.release();
        let (out, alive) = render_frames(&mut v, b, 2000);
        (v, out, alive)
    }

    #[test]
    fn released_voice_plays_past_loop_end_when_enabled() {
        let b = ones_then_twos();
        let (_, out, alive) = release_into(&b, true);
        // Frames 1500..=1999 are ones, 2000..=2999 twos, each scaled by the 10 s release.
        let level = |k: usize| 1.0 - k as f32 / 480_000.0;
        for (k, s) in out[..1500].iter().enumerate() {
            let want = if k < 500 { 1.0 } else { 2.0 } * level(k);
            assert!((s - want).abs() < 1e-5, "frame {k}: {s} want {want}");
        }
        assert!(!alive, "dies at End like a one-shot");
        assert!(out[1500..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn released_voice_stays_in_loop_when_disabled() {
        let b = ones_then_twos();
        let (v, out, alive) = release_into(&b, false);
        assert!(alive, "the 10 s release is still sounding");
        assert!(
            out.iter().all(|s| *s > 0.99 && *s <= 1.0),
            "never reads past the loop"
        );
        assert!(v.pos >= 1000.0 && v.pos < 2000.0, "pos {}", v.pos);
    }

    #[test]
    fn ping_pong_voice_turns_forward_on_release_past_loop() {
        let b = buf(ramp(3000), LoopMode::PingPong, 1000, 1099);
        let mut v = Voice::new(60, 1090, &b, RATE, adsr(0.0, 0.0, 1.0, 10_000.0), None)
            .release_past_loop(true);
        render_frames(&mut v, &b, 20);
        assert!(!v.forward, "moving backward after the reflection");
        let at_release = v.pos;
        v.release();
        let mut positions = vec![v.pos];
        loop {
            let (_, alive) = render_frames(&mut v, &b, 1);
            if !alive || positions.len() > 3000 {
                break;
            }
            positions.push(v.pos);
        }
        assert!(
            positions.windows(2).all(|w| w[1] == w[0] + 1.0),
            "turns forward at once and never wraps"
        );
        assert_eq!(positions.last().copied(), Some(2999.0), "plays on to End");
        assert_eq!(positions.len() as f64, 2999.0 - at_release + 1.0);
    }

    #[test]
    fn preview_voice_holds_level_through_release() {
        let b = ones(10_000, LoopMode::Forward, 100, 999);
        let mut v = Voice::new(60, 0, &b, RATE, adsr(0.0, 0.0, 1.0, 30.0), None).preview(true);
        render_frames(&mut v, &b, 500);
        v.release();
        // 100 ms later, three times the 30 ms release, it is still at full level.
        let (out, alive) = render_frames(&mut v, &b, 4800);
        assert!(alive);
        assert!(out.iter().all(|s| *s == 1.0));
        assert_eq!(v.pos, 5300.0, "left the loop");
        // Frames 5300..=9999 remain, then it stops at End.
        let (out, alive) = render_frames(&mut v, &b, 5000);
        assert!(!alive);
        assert!(out[..4700].iter().all(|s| *s == 1.0));
        assert!(out[4700..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_stolen_preview_voice_still_fades() {
        let b = ones(10_000, LoopMode::Forward, 100, 999);
        let mut v = Voice::new(60, 0, &b, RATE, adsr(0.0, 0.0, 1.0, 30.0), None).preview(true);
        render_frames(&mut v, &b, 500);
        v.fade_out(5.0);
        let (out, alive) = render_frames(&mut v, &b, 241);
        assert!(!alive, "gone after 5 ms");
        assert!(out.windows(2).all(|w| w[1] < w[0]));
    }

    #[test]
    fn ampeg_converts_seconds_and_percent() {
        let a = Ampeg {
            attack: 0.005,
            decay: 1.5,
            sustain: 40.0,
            release: 0.25,
        };
        let got = ampeg_to_adsr(&a);
        assert!((got.attack_ms - 5.0).abs() < 1e-4, "{got:?}");
        assert!((got.decay_ms - 1500.0).abs() < 1e-3, "{got:?}");
        assert!((got.sustain - 0.4).abs() < 1e-6, "{got:?}");
        assert!((got.release_ms - 250.0).abs() < 1e-3, "{got:?}");
        // The SFZ defaults: no attack, decay or release, full sustain.
        let zero = Ampeg {
            attack: 0.0,
            decay: 0.0,
            sustain: 100.0,
            release: 0.0,
        };
        assert_eq!(ampeg_to_adsr(&zero), adsr(0.0, 0.0, 1.0, 0.0));
        // Out of range values are clamped.
        let wild = Ampeg {
            attack: 20.0,
            decay: -1.0,
            sustain: 150.0,
            release: f32::NAN,
        };
        assert_eq!(ampeg_to_adsr(&wild), adsr(10_000.0, 0.0, 1.0, 30.0));
    }

    #[test]
    fn a_one_shot_region_voice_ignores_release_and_the_loop() {
        let b = ones_then_twos();
        let mut v = Voice::new(60, 1500, &b, RATE, FLAT, None).for_region(0.5, Playback::OneShot);
        render_frames(&mut v, &b, 10);
        v.release();
        assert!(!v.is_releasing());
        // 1510..=1999 at 0.5, then 2000..=2999 at 1.0, then it ends.
        let (out, alive) = render_frames(&mut v, &b, 1600);
        assert!(!alive);
        assert!(out[..490].iter().all(|s| *s == 0.5));
        assert!(out[490..1490].iter().all(|s| *s == 1.0));
        assert!(out[1490..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn adsr_clamped_limits_ranges_and_replaces_nan() {
        let a = Adsr {
            attack_ms: -5.0,
            decay_ms: 20_000.0,
            sustain: f32::NAN,
            release_ms: f32::NAN,
        }
        .clamped();
        assert_eq!(
            a,
            Adsr {
                attack_ms: 0.0,
                decay_ms: 10_000.0,
                sustain: 1.0,
                release_ms: 30.0,
            }
        );
        assert_eq!(
            Adsr {
                sustain: 1.5,
                ..Adsr::DEFAULT
            }
            .clamped()
            .sustain,
            1.0
        );
        assert_eq!(Adsr::DEFAULT.clamped(), Adsr::DEFAULT);
    }
}
