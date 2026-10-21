//! Generate tail (v0.3 §3–4): gives a cut-off sample a new ending made from its own sound.
//!
//! The **cut point** is the End marker. End is the last original frame kept, so the new audio
//! starts at `e.end + 1`; in this module `cut` is that first new frame (an exclusive bound on
//! the kept audio). The tail is analysed from the **source region** before the cut (§4.1–4.2)
//! and rendered by a method into a splice step that replaces everything from the start of its
//! join overlap to the end of the input:
//!
//! - **Decay** continues the sound at the level it has at the cut and fades it to silence.
//! - **Sustain** continues it at a steady level for the body length, with a seamless loop
//!   built in, then an optional release.
//!
//! `audio` is always the audio the new step will see: the original with `e.effects` applied.

mod granular;
mod harmonic;
pub mod measure;
mod smooth;

use std::sync::Arc;

use thiserror::Error;

use crate::dsp::envelope::{DecayFit, decay_fit, level_db};
use crate::dsp::pitch::{Character, character};
use crate::edit::{EditState, LoopMode};
use crate::effects::{AppliedEffect, Op, Splice, SpliceKind};

/// The automatic source region is at most this many seconds before the cut...
const SOURCE_S: f64 = 1.0;

/// ...and never includes this first fraction of Start…cut, which holds the attack.
const ATTACK_FRACTION: usize = 5;

/// The shortest usable source region, in seconds.
const MIN_SOURCE_S: f64 = 0.030;

/// A tonal source region must hold at least this many periods.
const MIN_SOURCE_PERIODS: f32 = 4.0;

/// The autocorrelation strength at which a source counts as tonal (v0.3 §4.2).
const TONAL_THRESHOLD: f32 = 0.6;

/// The level at the cut is the RMS of this many seconds before it.
const CUT_LEVEL_S: f64 = 0.020;

/// A source region quieter than this (dBFS RMS) is silent: there is nothing to continue.
const SILENT_DB: f32 = -120.0;

/// The decay time when the source is steady, in seconds to fall 60 dB (v0.3 §3.1).
const STEADY_DECAY_T60: f32 = 1.0;

/// The release time when the source is steady (v0.3 §3.2).
const STEADY_RELEASE_T60: f32 = 0.5;

/// The shortest decay time a setting can ask for, in seconds.
const MIN_T60: f32 = 0.01;

/// A decaying tail ends when its level falls to this (dBFS)...
const END_DB: f32 = -90.0;

/// ...or after this many seconds...
const MAX_TAIL_S: f32 = 30.0;

/// ...and its last this-many seconds ramp to exactly zero.
const END_RAMP_S: f64 = 0.005;

/// The range and the default of the sustain body length, in seconds.
const BODY_S: (f32, f32, f32) = (0.1, 30.0, 2.0);

/// What happens after the cut.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TailMode {
    /// Continue the sound and let it fade to silence.
    Decay,
    /// Hold the sound steady with a seamless loop, then an optional release.
    Sustain,
}

/// How the new audio is made from the source region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TailMethod {
    /// Stitches short patches of the source together (v0.3 §4.3).
    Granular,
    /// Freezes the spectrum at the cut and lets each frequency fade at its own rate (v0.3
    /// §4.4).
    Smooth,
    /// Rebuilds the sound from its partials as smooth decaying tones, plus a quiet noise layer
    /// for what they leave over (v0.3 §4.5).
    Harmonic,
}

/// What the user asks for. `None` means "use the fitted default".
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TailSettings {
    pub mode: TailMode,
    pub method: TailMethod,
    /// Decay mode: seconds to fall 60 dB. Default: the fitted decay, or 1.0 s when steady.
    pub decay_t60: Option<f32>,
    /// Sustain mode: the body length in seconds, 0.1–30.
    pub body_s: f32,
    /// Sustain mode: set loop 0 to the body.
    pub loop_on: bool,
    /// Sustain mode: seconds for the release to fall 60 dB; 0 means no release. Default: the
    /// fitted decay, or 0.5 s when steady.
    pub release_t60: Option<f32>,
    /// How much faster the highs die away, 0–1 (v0.3 §4.7). Sustain applies it to the release.
    pub darken: f32,
    /// Seeds every random choice, so the same settings always give the same audio.
    pub seed: u64,
}

impl Default for TailSettings {
    fn default() -> TailSettings {
        TailSettings {
            mode: TailMode::Decay,
            method: TailMethod::Granular,
            decay_t60: None,
            body_s: BODY_S.2,
            loop_on: true,
            release_t60: None,
            darken: 0.0,
            seed: 1,
        }
    }
}

/// What the analysis found in the source region (v0.3 §4.2).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TailAnalysis {
    /// The source region, inclusive.
    pub source: (usize, usize),
    /// Steady or decaying, and the recent rate of fall.
    pub fit: DecayFit,
    /// Tonal (with its period) or noisy, at the tail threshold of 0.6.
    pub character: Character,
    /// The RMS of the 20 ms before the cut, in dBFS. Every method starts its tail there.
    pub level_at_cut_db: f32,
    /// Decay for a decaying source, Sustain for a steady one.
    pub suggested_mode: TailMode,
    /// The method the dialog marks as suggested: Smooth for a tonal source, Granular for a
    /// noisy one.
    pub suggested_method: TailMethod,
}

impl TailAnalysis {
    /// The decay time used while the settings leave it unset: the fitted one, or 1.0 s when
    /// the source is steady (v0.3 §3.1).
    pub fn default_decay_t60(&self) -> f32 {
        self.fit.t60.unwrap_or(STEADY_DECAY_T60)
    }

    /// The release time used while the settings leave it unset: the fitted decay time, or
    /// 0.5 s when the source is steady (v0.3 §3.2).
    pub fn default_release_t60(&self) -> f32 {
        self.fit.t60.unwrap_or(STEADY_RELEASE_T60)
    }
}

/// Why no tail can be made. The messages are shown in the dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Error)]
pub enum TailError {
    #[error(
        "The source region is too short. Select a longer source region: at least 30 ms, \
         and four periods of a pitched sound."
    )]
    SourceTooShort,
    #[error("The audio before the cut is silent, so there is nothing to continue.")]
    Silent,
    #[error("There is no audio to continue.")]
    NoAudio,
}

/// Something the dialog points out without blocking Apply (v0.3 §7.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TailWarning {
    /// The seamless Granular loop did not fit the source, so the loop uses a crossfade.
    LoopCrossfadeFallback,
    /// The join overlap was shortened, or dropped, to stay clear of the existing loop.
    OverlapShortened,
    /// The source region is short, so repetition may be audible.
    ShortSource,
    /// The chosen method suits tonal sounds, and the analysis calls this source noisy.
    NoisySourceForMethod,
}

/// A rendered tail, ready to apply.
#[derive(Clone, PartialEq, Debug)]
pub struct TailRender {
    /// The splice step (kind `Tail`): it replaces from the start of the join overlap to the
    /// end of the input.
    pub step: AppliedEffect,
    /// The edit state to apply: `e` with `step` appended to its effects, End on the new last
    /// frame and, in Sustain with the loop on, loop 0 on the body (Forward, crossfade 0).
    pub edit: EditState,
    pub warnings: Vec<TailWarning>,
    pub analysis: TailAnalysis,
}

/// What a method hands back to [`render_tail`].
pub(crate) struct Generated {
    /// The replacement for `cut − overlap` to the end of the input: `overlap` frames that
    /// crossfade from the original, then the new audio. One `Vec` per channel.
    audio: Vec<Vec<f32>>,
    /// How many frames before the cut the join replaces.
    overlap: usize,
    /// Sustain: the body length. The body starts at the cut.
    body: usize,
    /// The loop crossfade the body needs: 0 when the loop is seamless.
    loop_crossfade: usize,
    warnings: Vec<TailWarning>,
}

/// The settings with defaults filled in from the analysis and every value made safe.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Resolved {
    mode: TailMode,
    decay_t60: f32,
    /// The body length asked for, in seconds, for methods that round it to their own hop.
    body_s: f32,
    /// The body length in frames.
    body: usize,
    release_t60: f32,
    darken: f32,
    seed: u64,
}

/// The shortest channel's length.
fn frame_count(audio: &[Vec<f32>]) -> usize {
    audio.iter().map(Vec::len).min().unwrap_or(0)
}

/// The cut as the first new frame: just after End, kept inside the file (`frames` > 0).
fn cut_of(e: &EditState, frames: usize) -> usize {
    e.end.min(frames - 1) + 1
}

/// Frames in `secs` seconds at `rate`, rounded down.
fn secs_to_frames(secs: f64, rate: u32) -> usize {
    (secs * f64::from(rate)) as usize
}

/// The source region before `cut` (v0.3 §4.1): the selection, clamped to end before the cut,
/// or else the last second before the cut, never including the first 20 % of Start…cut.
fn source_region(
    e: &EditState,
    selection: Option<(usize, usize)>,
    cut: usize,
    rate: u32,
) -> (usize, usize) {
    let last = cut - 1;
    match selection {
        Some((a, b)) => {
            let sel_last = a.max(b).min(last);
            (a.min(b).min(sel_last), sel_last)
        }
        None => {
            let start = e.start.min(last);
            let skip = start + (cut - start) / ATTACK_FRACTION;
            let back = cut.saturating_sub(secs_to_frames(SOURCE_S, rate));
            (back.max(skip).min(last), last)
        }
    }
}

/// Analyses the source region before the cut (`e.end + 1`), as v0.3 §4.1–4.2 describe.
///
/// `audio` is the audio after `e`'s effects. Errors: `NoAudio` for no channels, fewer than two
/// frames or a zero rate; `SourceTooShort` for a region under 30 ms, or under four periods of
/// a tonal sound; `Silent` for a region quieter than −120 dBFS.
pub fn analyse(
    audio: &[Vec<f32>],
    e: &EditState,
    selection: Option<(usize, usize)>,
    rate: u32,
) -> Result<TailAnalysis, TailError> {
    let frames = frame_count(audio);
    if frames < 2 || rate == 0 {
        return Err(TailError::NoAudio);
    }
    let cut = cut_of(e, frames);
    let (first, last) = source_region(e, selection, cut, rate);
    let n = last + 1 - first;
    if (n as f64) < MIN_SOURCE_S * f64::from(rate) {
        return Err(TailError::SourceTooShort);
    }
    if level_db(audio, first, last) < SILENT_DB {
        return Err(TailError::Silent);
    }
    let fit = decay_fit(audio, first, last, rate);
    let character = character(audio, first, last, rate, TONAL_THRESHOLD);
    if character
        .period
        .is_some_and(|p| (n as f32) < MIN_SOURCE_PERIODS * p)
    {
        return Err(TailError::SourceTooShort);
    }
    let n20 = secs_to_frames(CUT_LEVEL_S, rate).clamp(1, cut);
    Ok(TailAnalysis {
        source: (first, last),
        fit,
        character,
        level_at_cut_db: level_db(audio, cut - n20, cut - 1),
        suggested_mode: if fit.steady {
            TailMode::Sustain
        } else {
            TailMode::Decay
        },
        suggested_method: if character.period.is_some() {
            TailMethod::Smooth
        } else {
            TailMethod::Granular
        },
    })
}

/// Renders a tail with the settings `s` and packages it as a splice step and the edit state
/// that applies it. See [`TailRender`]. Fails as [`analyse`] does.
pub fn render_tail(
    audio: &[Vec<f32>],
    e: &EditState,
    selection: Option<(usize, usize)>,
    rate: u32,
    s: &TailSettings,
) -> Result<TailRender, TailError> {
    let analysis = analyse(audio, e, selection, rate)?;
    let frames = frame_count(audio);
    let cut = cut_of(e, frames);
    let resolved = resolve(s, &analysis, rate);
    let g = match s.method {
        TailMethod::Granular => granular::render(audio, cut, &analysis, &resolved, e, rate)?,
        TailMethod::Smooth => smooth::render(audio, cut, &analysis, &resolved, e, rate)?,
        TailMethod::Harmonic => harmonic::render(audio, cut, &analysis, &resolved, e, rate)?,
    };
    let first = cut - g.overlap;
    let len = frame_count(&g.audio);
    let step = AppliedEffect {
        op: Op::Splice(Arc::new(Splice {
            kind: SpliceKind::Tail,
            audio: g.audio,
            generated: (g.overlap, len - 1),
        })),
        first,
        last: frames - 1,
    };
    let mut edit = e.clone();
    edit.effects.push(step.clone());
    edit.end = first + len - 1;
    if edit.loop_mode == LoopMode::Off {
        // Kept, but inside the new length, as `EditState::clamped` keeps them.
        edit.loop_end = edit.loop_end.min(edit.end);
        edit.loop_start = edit.loop_start.min(edit.loop_end);
    }
    if s.mode == TailMode::Sustain && s.loop_on && g.body > 0 {
        edit.loop_mode = LoopMode::Forward;
        edit.loop_start = cut;
        edit.loop_end = cut + g.body - 1;
        edit.crossfade = g.loop_crossfade;
    }
    Ok(TailRender {
        step,
        edit,
        warnings: g.warnings,
        analysis,
    })
}

/// Fills in the defaults from the analysis and makes every setting safe: a missing or
/// unusable decay time takes the fitted one, the body is clamped to 0.1–30 s and Darken to
/// 0–1, and a negative release counts as none.
fn resolve(s: &TailSettings, a: &TailAnalysis, rate: u32) -> Resolved {
    let usable = |t: &f32| t.is_finite();
    let decay_t60 = s
        .decay_t60
        .filter(|t| usable(t) && *t > 0.0)
        .unwrap_or(a.default_decay_t60());
    let release_t60 = s
        .release_t60
        .filter(usable)
        .unwrap_or(a.default_release_t60());
    Resolved {
        mode: s.mode,
        decay_t60: decay_t60.max(MIN_T60),
        body_s: s.body_s,
        body: sustain_lengths(s.body_s, rate, 1),
        release_t60: release_t60.max(0.0),
        darken: if s.darken.is_finite() {
            s.darken.clamp(0.0, 1.0)
        } else {
            0.0
        },
        seed: s.seed,
    }
}

/// How many frames before `cut` the join may crossfade over (v0.3 §4.6). With the loop on, the
/// overlap never reaches back to the loop end: it is shortened to fit, but not below `min`.
/// `None` means even `min` does not fit, and the method splices at the cut with no overlap.
pub(crate) fn join_overlap(e: &EditState, cut: usize, wanted: usize, min: usize) -> Option<usize> {
    if e.loop_mode == LoopMode::Off {
        return Some(wanted);
    }
    // The frames after the loop end and before the cut.
    let room = cut.saturating_sub(e.loop_end.saturating_add(1));
    if wanted <= room {
        Some(wanted)
    } else if room >= min.clamp(1, wanted.max(1)) {
        Some(room)
    } else {
        None
    }
}

/// An exponential fade from `level` that falls 60 dB in `t60` seconds, `n` frames long.
pub(crate) fn decay_envelope(level: f32, t60: f32, rate: u32, n: usize) -> Vec<f32> {
    // dB per frame. Each frame's gain is computed directly in f64, so long tails do not drift.
    let per_frame = -60.0 / (f64::from(t60.max(MIN_T60)) * f64::from(rate.max(1)));
    (0..n)
        .map(|i| (f64::from(level) * 10f64.powf(per_frame * i as f64 / 20.0)) as f32)
        .collect()
}

/// The length of a fade from `level_db` that falls 60 dB in `t60` seconds, until it reaches
/// −90 dBFS or 30 s, in frames (at least `min`).
pub(crate) fn tail_length(level_db: f32, t60: f32, rate: u32, min: usize) -> usize {
    let secs = (t60 * (level_db - END_DB) / 60.0).clamp(0.0, MAX_TAIL_S);
    secs_to_frames(f64::from(secs), rate)
        .max(min)
        .min(secs_to_frames(f64::from(MAX_TAIL_S), rate))
}

/// Ramps the last 5 ms of `ch` (or all of it, if shorter) linearly from 1 to exactly zero.
pub(crate) fn end_ramp(ch: &mut [f32], rate: u32) {
    let n = secs_to_frames(END_RAMP_S, rate).clamp(1, ch.len().max(1));
    let start = ch.len().saturating_sub(n);
    for (i, v) in ch[start..].iter_mut().enumerate() {
        *v *= if n == 1 {
            0.0
        } else {
            (n - 1 - i) as f32 / (n - 1) as f32
        };
    }
}

/// The sustain body length in frames for `body_s` seconds (clamped to 0.1–30 s, 2 s if not a
/// number), rounded to a whole number of `hop`s (at least one).
pub(crate) fn sustain_lengths(body_s: f32, rate: u32, hop: usize) -> usize {
    let secs = if body_s.is_finite() {
        body_s.clamp(BODY_S.0, BODY_S.1)
    } else {
        BODY_S.2
    };
    let hop = hop.max(1);
    let hops = (f64::from(secs) * f64::from(rate) / hop as f64).round() as usize;
    hops.max(1) * hop
}

#[cfg(test)]
pub(crate) mod test_signals {
    //! The synthetic sources of v0.3 §8.1, shared by the tests of every method. Each is longer
    //! than its cut, so the real ending is there to compare with.

    use std::f32::consts::TAU;

    use super::{TailRender, TailSettings, render_tail};
    use crate::dsp::rng::SplitMix64;
    use crate::edit::EditState;
    use crate::effects::apply_effect;

    /// The rate of every test signal: low, so the debug-build tests stay quick.
    pub const RATE: u32 = 22_050;

    /// Frames in `secs` seconds at [`RATE`].
    pub fn at(secs: f32) -> usize {
        (secs * RATE as f32) as usize
    }

    /// A sum of `harmonics` harmonics of `f0` with amplitudes `amp(h)`, each delayed by
    /// `phase` radians, sampled at frame `i`.
    fn harmonic(
        i: usize,
        f0: f32,
        harmonics: usize,
        amp: impl Fn(usize) -> f32,
        phase: f32,
    ) -> f32 {
        let t = i as f32 / RATE as f32;
        (1..=harmonics)
            .map(|h| amp(h) * (TAU * f0 * h as f32 * t - phase).cos())
            .sum()
    }

    /// Uniform noise in −1..1.
    fn white(r: &mut SplitMix64) -> f32 {
        r.next_f32() * 2.0 - 1.0
    }

    /// 2 s of a 220 Hz tone with five harmonics, falling 60 dB in 1.2 s, with a little noise
    /// that falls with it. Cut at 0.6 s.
    pub fn decaying_tone() -> (Vec<Vec<f32>>, usize) {
        let mut r = SplitMix64::new(11);
        let x = (0..at(2.0))
            .map(|i| {
                let g = 10f32.powf(-60.0 / 1.2 * (i as f32 / RATE as f32) / 20.0);
                let tone = harmonic(i, 220.0, 5, |h| 1.0 / h as f32, 0.0);
                0.4 * g * (tone + 0.02 * white(&mut r))
            })
            .collect();
        (vec![x], at(0.6))
    }

    /// 1.5 s of a steady, pulse-like 97 Hz tone: 20 equal harmonics. Cut at 1.0 s.
    pub fn steady_tone() -> (Vec<Vec<f32>>, usize) {
        let x = (0..at(1.5))
            .map(|i| 0.5 * harmonic(i, 97.0, 20, |_| 1.0 / 20.0, 0.0))
            .collect();
        (vec![x], at(1.0))
    }

    /// 1.5 s of steady noise, lightly low-passed. Cut at 1.0 s.
    pub fn noise() -> (Vec<Vec<f32>>, usize) {
        let mut r = SplitMix64::new(5);
        let mut z = 0.0f32;
        let x = (0..at(1.5))
            .map(|_| {
                z = 0.5 * z + 0.5 * white(&mut r);
                0.4 * z
            })
            .collect();
        (vec![x], at(1.0))
    }

    /// 1.5 s of a steady stereo 150 Hz tone with six harmonics, the right channel 1 radian
    /// behind the left in every harmonic, plus a little independent noise. Cut at 1.0 s.
    pub fn stereo_tone() -> (Vec<Vec<f32>>, usize) {
        let mut r = SplitMix64::new(3);
        let amp = |h: usize| 1.0 / h as f32;
        let (l, rr) = (0..at(1.5))
            .map(|i| {
                let l = 0.3 * harmonic(i, 150.0, 6, amp, 0.0) + 0.01 * white(&mut r);
                let rt = 0.3 * harmonic(i, 150.0, 6, amp, 1.0) + 0.01 * white(&mut r);
                (l, rt)
            })
            .unzip();
        (vec![l, rr], at(1.0))
    }

    /// All four signals with their names.
    pub fn all() -> Vec<(&'static str, Vec<Vec<f32>>, usize)> {
        let named = |name, (a, c)| (name, a, c);
        vec![
            named("decaying tone", decaying_tone()),
            named("steady tone", steady_tone()),
            named("noise", noise()),
            named("stereo tone", stereo_tone()),
        ]
    }

    /// An edit state over `audio` cut at `cut`: End is the last frame kept, `cut − 1`.
    pub fn cut_at(audio: &[Vec<f32>], cut: usize) -> EditState {
        let mut e = EditState::default_for(audio[0].len());
        e.end = cut - 1;
        e
    }

    /// Renders with `s` and returns the result and the audio after the step, checking that
    /// the returned edit is valid, ends on the new last frame and holds the step.
    pub fn run(x: &[Vec<f32>], e: &EditState, s: &TailSettings) -> (TailRender, Vec<Vec<f32>>) {
        let r = render_tail(x, e, None, RATE, s).expect("renders");
        let mut out = x.to_vec();
        apply_effect(&mut out, &r.step);
        assert_eq!(out[0].len(), r.edit.end + 1, "End is the new last frame");
        assert_eq!(
            r.edit.clone().clamped(x[0].len()),
            r.edit,
            "the edit is valid"
        );
        assert_eq!(r.edit.effects.last(), Some(&r.step));
        (r, out)
    }

    /// The decay rate the tail is meant to have, in dB per second.
    pub fn tail_rate(r: &TailRender) -> f32 {
        60.0 / r.analysis.fit.t60.unwrap_or(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::test_signals::*;
    use super::*;

    #[test]
    fn analysis_suggests_mode_and_method() {
        let (x, cut) = decaying_tone();
        let a = analyse(&x, &cut_at(&x, cut), None, RATE).unwrap();
        assert_eq!(a.suggested_mode, TailMode::Decay);
        assert!(!a.fit.steady);
        let t60 = a.fit.t60.unwrap();
        assert!((t60 - 1.2).abs() < 0.15 * 1.2, "t60 {t60}");

        let (x, cut) = steady_tone();
        let a = analyse(&x, &cut_at(&x, cut), None, RATE).unwrap();
        assert_eq!(a.suggested_mode, TailMode::Sustain);
        assert!(a.character.period.is_some(), "{:?}", a.character);

        let (x, cut) = noise();
        let a = analyse(&x, &cut_at(&x, cut), None, RATE).unwrap();
        assert_eq!(a.suggested_mode, TailMode::Sustain);
        assert_eq!(a.character.period, None);
        assert_eq!(a.suggested_method, TailMethod::Granular);

        // The level at the cut is the RMS of the last 20 ms.
        let want = level_db(&x, cut - at(0.02), cut - 1);
        assert!((a.level_at_cut_db - want).abs() < 1e-4);
    }

    #[test]
    fn source_region_skips_the_attack_and_respects_the_selection() {
        let (x, _) = steady_tone();
        let expect = |start: usize, cut: usize| {
            let skip = start + (cut - start) / 5;
            (cut.saturating_sub(at(1.0)).max(skip), cut - 1)
        };
        // Cut at 1.0 s: the attack rule wins (0.2 s > 0 s).
        let cut = at(1.0);
        let a = analyse(&x, &cut_at(&x, cut), None, RATE).unwrap();
        assert_eq!(a.source, expect(0, cut));
        assert_eq!(a.source, (at(1.0) / 5, cut - 1));
        // Cut at 1.4 s: the last second wins.
        let cut = at(1.4);
        let a = analyse(&x, &cut_at(&x, cut), None, RATE).unwrap();
        assert_eq!(a.source, expect(0, cut));
        assert_eq!(a.source.0, cut - at(1.0));
        // A later Start moves the attack rule with it.
        let mut e = cut_at(&x, cut);
        e.start = at(0.9);
        let a = analyse(&x, &e, None, RATE).unwrap();
        assert_eq!(a.source, expect(at(0.9), cut));
        assert!(a.source.0 > cut - at(1.0));

        // A selection is used as it is, in either order, and clamped to end before the cut.
        let cut = at(1.0);
        let e = cut_at(&x, cut);
        let a = analyse(&x, &e, Some((at(0.3), at(0.7))), RATE).unwrap();
        assert_eq!(a.source, (at(0.3), at(0.7)));
        let a = analyse(&x, &e, Some((at(1.3), at(0.5))), RATE).unwrap();
        assert_eq!(a.source, (at(0.5), cut - 1));
    }

    #[test]
    fn silent_or_short_source_is_refused() {
        let s = TailSettings::default();
        let zeros = vec![vec![0.0f32; at(1.0)]];
        let e = cut_at(&zeros, at(0.8));
        assert_eq!(analyse(&zeros, &e, None, RATE), Err(TailError::Silent));
        assert_eq!(
            render_tail(&zeros, &e, None, RATE, &s).unwrap_err(),
            TailError::Silent
        );

        let (x, cut) = steady_tone();
        let e = cut_at(&x, cut);
        let short = Some((cut - at(0.02), cut - 1));
        assert_eq!(analyse(&x, &e, short, RATE), Err(TailError::SourceTooShort));
        assert_eq!(
            render_tail(&x, &e, short, RATE, &s).unwrap_err(),
            TailError::SourceTooShort
        );
        // 40 ms is long enough in time, but holds under four periods of 97 Hz.
        let few = Some((cut - at(0.04), cut - 1));
        assert_eq!(analyse(&x, &e, few, RATE), Err(TailError::SourceTooShort));

        let one = vec![vec![0.5f32]];
        let e1 = EditState::default_for(1);
        assert_eq!(analyse(&one, &e1, None, RATE), Err(TailError::NoAudio));
        assert_eq!(
            render_tail(&one, &e1, None, RATE, &s).unwrap_err(),
            TailError::NoAudio
        );
        assert_eq!(analyse(&[], &e1, None, RATE), Err(TailError::NoAudio));
        assert_eq!(analyse(&x, &e, None, 0), Err(TailError::NoAudio));

        // A sound that is already silent at the cut, and settings that are not numbers, still
        // give finite audio.
        let mut quiet = x.clone();
        quiet[0][cut - at(0.05)..].iter_mut().for_each(|v| *v = 0.0);
        let odd = TailSettings {
            decay_t60: Some(f32::NAN),
            body_s: f32::NAN,
            release_t60: Some(f32::NAN),
            darken: f32::NAN,
            ..TailSettings::default()
        };
        for (mode, method) in [TailMode::Decay, TailMode::Sustain]
            .into_iter()
            .flat_map(|m| {
                [
                    TailMethod::Granular,
                    TailMethod::Smooth,
                    TailMethod::Harmonic,
                ]
                .map(|k| (m, k))
            })
        {
            for audio in [&x, &quiet] {
                let s = TailSettings {
                    mode,
                    method,
                    ..odd
                };
                let r = render_tail(audio, &e, None, RATE, &s).unwrap();
                let Op::Splice(sp) = &r.step.op else {
                    panic!("not a splice")
                };
                assert!(sp.audio.iter().flatten().all(|v| v.is_finite()));
            }
        }
    }

    #[test]
    fn join_overlap_stays_clear_of_the_loop() {
        let mut e = EditState::default_for(10_000);
        // Loop off: anything goes.
        assert_eq!(join_overlap(&e, 5000, 300, 50), Some(300));
        e.loop_mode = LoopMode::Forward;
        e.loop_start = 1000;
        e.loop_end = 4000;
        // 999 frames after the loop end and before the cut: room enough.
        assert_eq!(join_overlap(&e, 5000, 300, 50), Some(300));
        // Room for 99: shortened.
        e.loop_end = 4900;
        assert_eq!(join_overlap(&e, 5000, 300, 50), Some(99));
        // Room for 30, under the minimum: no overlap.
        e.loop_end = 4969;
        assert_eq!(join_overlap(&e, 5000, 300, 50), None);
        // The loop end right at the cut: no overlap.
        e.loop_end = 4999;
        assert_eq!(join_overlap(&e, 5000, 300, 50), None);
    }

    #[test]
    fn shared_helpers() {
        let env = decay_envelope(0.5, 1.0, 1000, 1001);
        assert_eq!(env.len(), 1001);
        assert!((env[0] - 0.5).abs() < 1e-7);
        // 60 dB down after 1 s.
        assert!((env[1000] / 0.5 - 1e-3).abs() < 1e-6);

        let mut a = vec![-1.0f32; 1000];
        end_ramp(&mut a, 1000);
        assert_eq!(a[999], 0.0);
        assert_eq!(a[995], -1.0);
        assert_eq!(a[997], -0.5);
        let mut tiny = vec![1.0f32; 2];
        end_ramp(&mut tiny, 1000);
        assert_eq!(tiny, [1.0, 0.0]);
        end_ramp(&mut [], 1000);

        assert_eq!(sustain_lengths(2.0, 1000, 1), 2000);
        assert_eq!(sustain_lengths(2.0, 1000, 64), 1984);
        // Clamped to 0.1–30 s; never 0.
        assert_eq!(sustain_lengths(0.0, 1000, 1), 100);
        assert_eq!(sustain_lengths(100.0, 1000, 1), 30_000);
        assert_eq!(sustain_lengths(0.1, 1000, 512), 512);
    }
}
