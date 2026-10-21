//! Make loop from spectrum (v0.3 §5).
//!
//! The effect averages the spectrum of a range (the selection, or Start…End) and replaces the
//! range with a seamless loop made by one inverse FFT, so it is exactly periodic by
//! construction. An entry transition inside the start of the range turns the original into the
//! loop, and an exit transition over the start of the audio after the range turns the loop's
//! continuation back into the original:
//!
//! ```text
//! before: [ audio before ][ source range a … b      ][ audio after b …        ]
//! after:  [ audio before ][ entry T ][ loop L ][ exit Tx ][ audio after b + Tx … ]
//! ```
//!
//! Here `b` is the first frame after the (inclusive) source range, so the step replaces
//! `a ..= b + Tx − 1` and the length changes by `T + L − (b − a)`.
//!
//! The loop has one of three flavours:
//! - **Plain** (Pitch lock off, or no pitch): the averaged spectrum with random phases.
//! - **Steady** (Detune 0): each harmonic one exact tone, a whole number of cycles per loop,
//!   with the phases the entry glide arrives at.
//! - **Ensemble** (Detune > 0): each harmonic spread into a Gaussian Detune cents wide, with
//!   random phases and a quiet floor from the averaged spectrum.
//!
//! This code is separate from the tail engine; it shares only the `dsp` building blocks.

use std::f64::consts::{FRAC_PI_2, PI, TAU};
use std::sync::Arc;

use thiserror::Error;

use crate::dsp::lsq::HarmonicFitter;
use crate::dsp::pitch::character;
use crate::dsp::rng::SplitMix64;
use crate::dsp::spectrum::{Complex32, Complex64, Fft, frame_size_for, hann, welch_power};
use crate::edit::{EditState, LoopMode};
use crate::effects::{AppliedEffect, Op, Splice, SpliceKind};

/// The smallest and largest analysis FFT sizes.
const MIN_FFT: usize = 256;
const MAX_FFT: usize = 65_536;

/// Auto FFT size: at most ~186 ms, and at least 1.5 frames in the range.
const AUTO_FFT_S: f32 = 0.186;
const AUTO_FFT_MIN_FRAMES: f32 = 1.5;

/// Fewer analysis frames than this gives [`SpectrumLoopWarning::FewFrames`].
const FEW_FRAMES: usize = 3;

/// The tonal threshold for spectrum loops, lower than for tails because the whole range is
/// averaged (v0.3 §5.1).
const TONAL_THRESHOLD: f32 = 0.45;

/// The pitch is refined from this many harmonics of the averaged spectrum.
const REFINE_HARMONICS: usize = 8;

/// A harmonic is used only within 70 dB (in power) of the loudest one.
const HARMONIC_RANGE: f64 = 1e-7;

/// Steady's amplitude window: four periods or 50 ms, whichever is longer.
const FIT_PERIODS: f64 = 4.0;
const FIT_MIN_S: f64 = 0.05;

/// Steady averages at most this many windows, evenly spread over the consecutive ones, so a
/// long range does not make the fits slow.
const MAX_FIT_WINDOWS: usize = 16;

/// Ensemble's floor from the averaged spectrum: −60 dB, in power.
const ENSEMBLE_FLOOR: f64 = 1e-6;

/// Ensemble's Gaussians are evaluated this many standard deviations either side.
const GAUSS_SPAN: f64 = 5.0;

/// The loop's peak limit, −0.2 dBFS.
const PEAK_LIMIT_DB: f64 = -0.2;

/// A range whose RMS is at or below this (−160 dBFS) is silent.
const SILENT_RMS: f64 = 1e-8;

/// The setting ranges (v0.3 §5.2).
const LOOP_S: (f32, f32) = (0.1, 30.0);
const TRANSITION_MS: (f32, f32) = (10.0, 1000.0);
const MAX_DETUNE_CENTS: f32 = 50.0;

/// What the user sets in the dialog (v0.3 §5.2). Out-of-range values are clamped, and
/// non-finite ones take the default.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SpectrumLoopSettings {
    /// The analysis FFT size, a power of two from 256 to 65536; `None` picks it automatically.
    /// It is never larger than the range.
    pub fft_size: Option<usize>,
    /// Build the loop from the harmonics of the pitch found (Steady or Ensemble). Plain is used
    /// when this is off or no pitch is found.
    pub pitch_lock: bool,
    /// 0–50 cents: 0 gives Steady, more gives Ensemble.
    pub detune_cents: f32,
    /// The loop length in seconds, 0.1–30.
    pub loop_s: f32,
    /// The entry and exit transition length in milliseconds, 10–1000, and at most half the
    /// range. The exit is also limited to the audio after the range.
    pub transition_ms: f32,
    /// The seed of the random phases (Plain and Ensemble). "New variation" picks a new one.
    pub seed: u64,
}

impl Default for SpectrumLoopSettings {
    fn default() -> Self {
        SpectrumLoopSettings {
            fft_size: None,
            pitch_lock: true,
            detune_cents: 0.0,
            loop_s: 3.0,
            transition_ms: 100.0,
            seed: 1,
        }
    }
}

/// What the dialog shows about the source range.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SpectrumAnalysis {
    /// The source range, inclusive.
    pub range: (usize, usize),
    /// The analysis FFT size used.
    pub fft_size: usize,
    /// The analysis bin width in Hz.
    pub bin_hz: f32,
    /// The number of analysis frames averaged.
    pub frames: usize,
    /// The pitch found, refined from the averaged spectrum; `None` when the range is not tonal.
    pub pitch_hz: Option<f32>,
}

/// Why no loop can be made. The messages are shown in the dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Error)]
pub enum SpectrumLoopError {
    #[error("The range is too short. Select at least 256 frames.")]
    RangeTooShort,
    #[error("The range is silent, so there is no spectrum to loop.")]
    Silent,
    #[error("There is no audio to loop.")]
    NoAudio,
}

/// Something the dialog points out without blocking Apply (v0.3 §5.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpectrumLoopWarning {
    /// Fewer than three analysis frames fit in the range, so the average is rough.
    FewFrames,
    /// The loop's bin width (rate ÷ loop length) is wider than the detune spread at the
    /// fundamental, so the detune cannot be resolved.
    LoopTooShortForDetune,
    /// Pitch lock is on but no pitch was found, so Plain was used.
    NoPitch,
}

/// A rendered spectrum loop, ready to apply.
#[derive(Clone, PartialEq, Debug)]
pub struct SpectrumLoopRender {
    /// The splice step (kind `SpectrumLoop`): it replaces `a ..= b + Tx − 1` with the entry
    /// transition, the loop and the exit transition, all of which are marked generated.
    pub step: AppliedEffect,
    /// The edit state to apply: `e` with `step` appended to its effects, loop 0 on the loop
    /// (Forward, crossfade 0) and the markers moved as in v0.3 §6.
    pub edit: EditState,
    pub warnings: Vec<SpectrumLoopWarning>,
    pub analysis: SpectrumAnalysis,
}

/// The analysis behind [`SpectrumAnalysis`], with what the loop is built from.
struct Analysis {
    summary: SpectrumAnalysis,
    /// The Welch power per channel, `fft_size / 2 + 1` bins.
    power: Vec<Vec<f64>>,
    /// `power` summed over the channels.
    total: Vec<f64>,
    /// The range's RMS over all channels.
    rms: f64,
    /// The frames in the shortest channel.
    frames: usize,
}

/// The largest power of two that is at most `n` (`n ≥ 1`).
fn pow2_floor(n: usize) -> usize {
    1 << (usize::BITS - 1 - n.max(1).leading_zeros())
}

/// Analyses the source range: the selection (ordered and clamped to the file) or Start…End.
///
/// - The FFT size is the one asked for, made a power of two from 256 to 65536, or else the
///   largest power of two ≤ the range length ÷ 1.5 and ≤ 186 ms. It is never larger than the
///   range.
/// - The spectrum is the Welch average of Hann frames with a hop of a quarter frame.
/// - The pitch comes from the autocorrelation with threshold 0.45, refined from the averaged
///   spectrum by parabolic interpolation on the first eight harmonics, weighted by strength.
pub fn analyse_range(
    audio: &[Vec<f32>],
    e: &EditState,
    selection: Option<(usize, usize)>,
    rate: u32,
    fft_size: Option<usize>,
) -> Result<SpectrumAnalysis, SpectrumLoopError> {
    analyse(audio, e, selection, rate, fft_size).map(|an| an.summary)
}

fn analyse(
    audio: &[Vec<f32>],
    e: &EditState,
    selection: Option<(usize, usize)>,
    rate: u32,
    fft_size: Option<usize>,
) -> Result<Analysis, SpectrumLoopError> {
    let frames = audio.iter().map(Vec::len).min().unwrap_or(0);
    if frames == 0 || rate == 0 {
        return Err(SpectrumLoopError::NoAudio);
    }
    let (p, q) = selection.unwrap_or((e.start, e.end));
    let (a, last) = (p.min(q).min(frames - 1), p.max(q).min(frames - 1));
    let len = last + 1 - a;
    if len < MIN_FFT {
        return Err(SpectrumLoopError::RangeTooShort);
    }
    let energy: f64 = audio
        .iter()
        .flat_map(|ch| &ch[a..=last])
        .map(|&v| f64::from(v) * f64::from(v))
        .sum();
    let rms = (energy / (len * audio.len()) as f64).sqrt();
    if !rms.is_finite() || rms <= SILENT_RMS {
        return Err(SpectrumLoopError::Silent);
    }

    let wanted =
        fft_size.unwrap_or_else(|| frame_size_for(len, rate, AUTO_FFT_S, AUTO_FFT_MIN_FRAMES));
    let n = pow2_floor(wanted.clamp(MIN_FFT, MAX_FFT)).min(pow2_floor(len));
    let power: Vec<Vec<f64>> = audio
        .iter()
        .map(|ch| {
            welch_power(ch, a, last, n)
                .into_iter()
                .map(f64::from)
                .collect()
        })
        .collect();
    let mut total = vec![0.0; n / 2 + 1];
    for p in &power {
        total.iter_mut().zip(p).for_each(|(t, v)| *t += v);
    }

    let rate_f = f64::from(rate);
    let pitch_hz = character(audio, a, last, rate, TONAL_THRESHOLD)
        .period
        .map(|period| rate_f / f64::from(period))
        .map(|f0| refine_pitch(&total, n, rate_f, f0))
        .filter(|f0| f0.is_finite() && *f0 > 0.0)
        .map(|f0| f0 as f32);
    Ok(Analysis {
        summary: SpectrumAnalysis {
            range: (a, last),
            fft_size: n,
            bin_hz: (rate_f / n as f64) as f32,
            frames: (len - n) / (n / 4) + 1,
            pitch_hz,
        },
        power,
        total,
        rms,
        frames,
    })
}

/// The loudest bin of `p` within `span` bins of the fractional bin `centre`, and its position
/// refined by parabolic interpolation on the log power. `None` when that window is not
/// inside the spectrum, away from both ends.
fn peak_near(p: &[f64], centre: f64, span: f64) -> Option<(usize, f64)> {
    let top = p.len().checked_sub(2)?;
    let end = centre + span;
    if end.is_nan() || end >= top as f64 {
        return None;
    }
    let lo = (centre - span).floor().max(1.0) as usize;
    let hi = ((centre + span).ceil() as usize).min(top);
    if hi < lo {
        return None;
    }
    let k = (lo..=hi).fold(lo, |best, i| if p[i] > p[best] { i } else { best });
    let ln = |i: usize| p[i].max(f64::MIN_POSITIVE).ln();
    let (x, y, z) = (ln(k - 1), ln(k), ln(k + 1));
    let curve = x - 2.0 * y + z;
    let d = if curve < 0.0 {
        (0.5 * (x - z) / curve).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    Some((k, k as f64 + d))
}

/// How far either side of a harmonic's expected bin its peak is searched for: a quarter of the
/// fundamental, at least one bin.
fn search_span(bins_per_f0: f64) -> f64 {
    (bins_per_f0 / 4.0).max(1.0)
}

/// `f0` refined from the averaged spectrum `total` of an `n`-point analysis: the
/// strength-weighted mean of `f_h / h` over the first eight harmonic peaks (v0.3 §5.1).
fn refine_pitch(total: &[f64], n: usize, rate: f64, f0: f64) -> f64 {
    let per = f0 * n as f64 / rate;
    let span = search_span(per);
    let (mut num, mut den) = (0.0, 0.0);
    for h in 1..=REFINE_HARMONICS {
        let Some((k, pos)) = peak_near(total, h as f64 * per, span) else {
            break;
        };
        let w = total[k].sqrt();
        num += w * pos * rate / n as f64 / h as f64;
        den += w;
    }
    let f = num / den;
    if den > 0.0 && f.is_finite() { f } else { f0 }
}

/// The harmonics of `f0` found in the averaged spectrum, as `(h, peak bin)`, from the
/// fundamental up to half a fundamental below the Nyquist frequency, at most `max` of them,
/// keeping only those within 70 dB of the loudest.
fn harmonic_peaks(total: &[f64], n: usize, rate: f64, f0: f64, max: usize) -> Vec<(usize, usize)> {
    let per = f0 * n as f64 / rate;
    let span = search_span(per);
    let mut found = Vec::new();
    for h in 1..=max {
        if h as f64 * f0 >= rate / 2.0 - f0 / 2.0 {
            break;
        }
        match peak_near(total, h as f64 * per, span) {
            Some((k, _)) => found.push((h, k)),
            None => break,
        }
    }
    let loudest = found.iter().map(|&(_, k)| total[k]).fold(0.0, f64::max);
    found.retain(|&(_, k)| total[k] > 0.0 && total[k] >= loudest * HARMONIC_RANGE);
    found
}

/// A raised-cosine ramp from 0 to 1 over `n` frames, sampled at the frame centres.
fn smooth_step(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (PI * (i as f64 + 0.5) / n as f64).cos())
        .collect()
}

/// `v` wrapped into −π..π.
fn wrap_phase(v: f64) -> f64 {
    (v + PI).rem_euclid(TAU) - PI
}

/// The generated audio, one `Vec` per channel in each part.
struct Parts {
    entry: Vec<Vec<f32>>,
    looped: Vec<Vec<f32>>,
    exit: Vec<Vec<f32>>,
}

/// Everything the flavours share.
struct Job<'a> {
    audio: &'a [Vec<f32>],
    an: &'a Analysis,
    rate: f64,
    /// The first frame of the source range, and the first frame after it.
    a: usize,
    b: usize,
    /// The loop, entry and exit lengths.
    l: usize,
    t: usize,
    tx: usize,
}

impl Job<'_> {
    /// The gain that brings `looped` to the source range's RMS, lowered if the peak would
    /// pass −0.2 dBFS. `None` when the loop is silent.
    fn level_gain(&self, looped: &[Vec<f32>]) -> Option<f64> {
        let (mut energy, mut peak) = (0.0f64, 0.0f64);
        for v in looped.iter().flatten() {
            let v = f64::from(*v);
            energy += v * v;
            peak = peak.max(v.abs());
        }
        let rms = (energy / (self.l * looped.len()) as f64).sqrt();
        if !rms.is_finite() || rms <= 0.0 {
            return None;
        }
        let gain = self.an.rms / rms;
        let limit = 10f64.powf(PEAK_LIMIT_DB / 20.0);
        Some(if peak * gain > limit {
            limit / peak
        } else {
            gain
        })
    }

    /// The equal-power exit: the loop's continuation crossfades into the original after `b`.
    fn crossfade_exit(&self, looped: &[Vec<f32>]) -> Vec<Vec<f32>> {
        let g = smooth_step(self.tx);
        looped
            .iter()
            .zip(self.audio)
            .map(|(lp, x)| {
                (0..self.tx)
                    .map(|j| {
                        let (sin, cos) = (g[j] * FRAC_PI_2).sin_cos();
                        (f64::from(lp[j % self.l]) * cos + f64::from(x[self.b + j]) * sin) as f32
                    })
                    .collect()
            })
            .collect()
    }
}

/// Steady (v0.3 §5.1, §5.3): each harmonic `hs` of `f0` becomes one exact tone at a multiple
/// of the loop's rounded fundamental, `m` cycles per loop. Every harmonic up to the Nyquist
/// frequency is kept.
///
/// - Amplitudes: the RMS over consecutive windows (four periods or 50 ms) across the range of
///   joint least-squares fits of all the harmonics.
/// - Entry: a fit at `a` gives the original's harmonics; each glides on a raised cosine to the
///   loop's amplitude while the fundamental glides from `f0` to the rounded pitch, and the
///   residual fades out. The loop's phases are the ones the glide arrives at.
/// - Exit: the reverse glide into a fit of the original at `b`, with a shortest-way phase
///   correction spread over it, while the residual fades in. With less than one fit window
///   after `b` it is the Plain crossfade instead.
fn steady(job: &Job, f0: f64, m: usize, hs: &[usize]) -> Result<Parts, SpectrumLoopError> {
    let Job {
        audio,
        rate,
        a,
        b,
        l,
        t,
        tx,
        ..
    } = *job;
    let frames = job.an.frames;
    let f0q = m as f64 * rate / l as f64;
    let w = ((FIT_PERIODS * rate / f0).max(FIT_MIN_S * rate).round() as usize).max(1);
    let fitter = |len: usize| HarmonicFitter::new(len, f0, hs, rate as u32);

    // Amplitudes: RMS of the fits over consecutive windows, at most MAX_FIT_WINDOWS of them.
    let len = b - a;
    let wl = w.min(len);
    let count = len / wl;
    let starts: Vec<usize> = if count <= MAX_FIT_WINDOWS {
        (0..count).map(|i| a + i * wl).collect()
    } else {
        (0..MAX_FIT_WINDOWS)
            .map(|i| a + i * (count - 1) / (MAX_FIT_WINDOWS - 1) * wl)
            .collect()
    };
    let window_fit = fitter(wl);
    let full_fit = if wl == w { None } else { Some(fitter(w)) };
    let full_fit = full_fit.as_ref().unwrap_or(&window_fit);
    let mut amps: Vec<Vec<f64>> = audio
        .iter()
        .map(|ch| {
            let windows: Vec<&[f32]> = starts.iter().map(|&s| &ch[s..s + wl]).collect();
            let mut sum = vec![0.0; hs.len()];
            for c in window_fit.fit(&windows) {
                sum.iter_mut()
                    .zip(&c)
                    .for_each(|(acc, c)| *acc += c.norm_sqr());
            }
            sum.iter()
                .map(|e| (e / starts.len() as f64).sqrt())
                .collect()
        })
        .collect();

    // The original's harmonics at the start of the entry (one fit window, or what is left of
    // the file, read as zero-padded by the fitter only when the file is shorter than that).
    let fit_in: Vec<Vec<Complex64>> = audio
        .iter()
        .map(|ch| {
            let x = &ch[a..a + w.min(frames - a)];
            if x.len() == w {
                full_fit.fit(&[x]).remove(0)
            } else {
                fitter(x.len()).fit(&[x]).remove(0)
            }
        })
        .collect();

    // The fundamental's phase through the entry glide, from 0 at `a`.
    let g = smooth_step(t);
    let mut phi = vec![0.0; t + 1];
    for i in 0..t {
        phi[i + 1] = phi[i] + TAU * ((1.0 - g[i]) * f0 + g[i] * f0q) / rate;
    }
    // Each harmonic's phase where the loop starts.
    let start_phase: Vec<Vec<f64>> = fit_in
        .iter()
        .map(|c| {
            c.iter()
                .zip(hs)
                .map(|(c, &h)| c.arg() + h as f64 * phi[t])
                .collect()
        })
        .collect();

    // The loop: one inverse FFT with each harmonic on bin h·m.
    let fft = Fft::new(l);
    let build = |amps: &[Vec<f64>]| -> Vec<Vec<f32>> {
        amps.iter()
            .zip(&start_phase)
            .map(|(amp, ph)| {
                let mut spec = vec![Complex32::default(); l / 2 + 1];
                for ((&h, &a), &p) in hs.iter().zip(amp).zip(ph) {
                    spec[h * m] = Complex32::from_polar(
                        (a * l as f64 / 2.0) as f32,
                        p.rem_euclid(TAU) as f32,
                    );
                }
                fft.inverse(&spec)
            })
            .collect()
    };
    let unit = build(&amps);
    let gain = job.level_gain(&unit).ok_or(SpectrumLoopError::Silent)?;
    amps.iter_mut().flatten().for_each(|v| *v *= gain);
    let looped = build(&amps);

    // Sums of many harmonics are evaluated with powers of one rotation per frame, not one
    // cosine per harmonic: `powers(θ)[h] = e^{i·h·θ}`.
    let top = hs.iter().copied().max().unwrap_or(0);
    let mut pa = vec![Complex64::default(); top + 1];
    let mut pb = vec![Complex64::default(); top + 1];
    let powers = |out: &mut [Complex64], theta: f64| {
        let step = Complex64::from_polar(1.0, theta);
        let mut z = Complex64::new(1.0, 0.0);
        for p in out {
            *p = z;
            z *= step;
        }
    };
    let w0 = TAU * f0 / rate;

    // The original's phase at `a` of each harmonic, as a unit rotation.
    let units_in: Vec<Vec<Complex64>> = fit_in
        .iter()
        .map(|c| {
            c.iter()
                .map(|c| Complex64::from_polar(1.0, c.arg()))
                .collect()
        })
        .collect();
    let mut entry = vec![Vec::with_capacity(t); audio.len()];
    for i in 0..t {
        // The original's model runs at f0; the glide at the gliding fundamental.
        powers(&mut pa, w0 * i as f64);
        powers(&mut pb, phi[i]);
        for (((out, x), (c_in, units)), amp) in entry
            .iter_mut()
            .zip(audio)
            .zip(fit_in.iter().zip(&units_in))
            .zip(&amps)
        {
            let (mut model, mut glide) = (0.0, 0.0);
            for (((c, unit), &h), &target) in c_in.iter().zip(units).zip(hs).zip(amp) {
                model += (c * pa[h]).re;
                let level = (1.0 - g[i]) * c.norm() + g[i] * target;
                glide += level * (unit * pb[h]).re;
            }
            let resid = f64::from(x[a + i]) - model;
            out.push((glide + (1.0 - g[i]) * resid) as f32);
        }
    }

    let exit = if tx == 0 {
        vec![Vec::new(); audio.len()]
    } else if frames - b < w {
        job.crossfade_exit(&looped)
    } else {
        let gx = smooth_step(tx);
        let mut psi = vec![0.0; tx + 1];
        for j in 0..tx {
            psi[j + 1] = psi[j] + TAU * ((1.0 - gx[j]) * f0q + gx[j] * f0) / rate;
        }
        let fit_out: Vec<Vec<Complex64>> = audio
            .iter()
            .map(|x| full_fit.fit(&[&x[b..b + w]]).remove(0))
            .collect();
        // The shortest-way correction that lands each harmonic on the original's phase at
        // the last exit frame.
        let delta: Vec<Vec<f64>> = fit_out
            .iter()
            .zip(&start_phase)
            .map(|(c_out, start)| {
                c_out
                    .iter()
                    .zip(hs)
                    .zip(start)
                    .map(|((c, &h), &s)| {
                        let h = h as f64;
                        let target = c.arg() + h * w0 * (tx - 1) as f64;
                        wrap_phase(target - (s + h * psi[tx - 1]))
                    })
                    .collect()
            })
            .collect();
        let mut exit = vec![Vec::with_capacity(tx); audio.len()];
        for j in 0..tx {
            powers(&mut pa, w0 * j as f64);
            powers(&mut pb, psi[j]);
            for ((((out, x), c_out), (start, d)), amp) in exit
                .iter_mut()
                .zip(audio)
                .zip(&fit_out)
                .zip(start_phase.iter().zip(&delta))
                .zip(&amps)
            {
                let (mut model, mut glide) = (0.0, 0.0);
                for ((((c, &h), &s), &d), &from) in c_out.iter().zip(hs).zip(start).zip(d).zip(amp)
                {
                    model += (c * pa[h]).re;
                    let level = (1.0 - gx[j]) * from + gx[j] * c.norm();
                    glide += level * (Complex64::from_polar(1.0, s + gx[j] * d) * pb[h]).re;
                }
                let resid = f64::from(x[b + j]) - model;
                out.push((glide + gx[j] * resid) as f32);
            }
        }
        exit
    };
    Ok(Parts {
        entry,
        looped,
        exit,
    })
}

/// The averaged power of each channel moved onto the loop's grid of `l / 2 + 1` bins, scaled
/// so that a stationary noise keeps its density: bin `k` of the loop is at `k · n / l` of the
/// analysis, linearly interpolated, times `l / Σw²` for the Hann window `w`.
fn loop_grid_power(an: &Analysis, l: usize) -> Vec<Vec<f64>> {
    let n = an.summary.fft_size;
    let sum_w2: f64 = hann(n).iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    an.power
        .iter()
        .map(|p| {
            (0..=l / 2)
                .map(|k| {
                    let pos = k as f64 * n as f64 / l as f64;
                    let i = (pos.floor() as usize).min(p.len() - 1);
                    let j = (i + 1).min(p.len() - 1);
                    let f = pos - i as f64;
                    (p[i] * (1.0 - f) + p[j] * f) * l as f64 / sum_w2
                })
                .collect()
        })
        .collect()
}

/// Ensemble's power on the loop grid: each harmonic of `f0` keeps its energy but is spread
/// into a Gaussian whose width is `cents` at its own frequency (never narrower than one loop
/// bin), plus the averaged spectrum at −60 dB as a floor.
fn ensemble_power(an: &Analysis, l: usize, rate: f64, f0: f64, cents: f64) -> Vec<Vec<f64>> {
    let n = an.summary.fft_size;
    let sum_w2: f64 = hann(n).iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    let mut out: Vec<Vec<f64>> = loop_grid_power(an, l)
        .into_iter()
        .map(|d| d.into_iter().map(|v| v * ENSEMBLE_FLOOR).collect())
        .collect();
    // A harmonic's energy is summed over its main lobe, without reaching its neighbours.
    let per = f0 * n as f64 / rate;
    let half = ((per - 1.0) / 2.0).floor().clamp(0.0, 2.0) as usize;
    // One-sided analysis energy to loop-grid energy, for a tone: l² / (n · Σw²).
    let to_loop = (l as f64).powi(2) / (n as f64 * sum_w2);
    let bin_hz = rate / l as f64;
    let top = l / 2;
    for (h, k) in harmonic_peaks(&an.total, n, rate, f0, usize::MAX) {
        let fc = h as f64 * f0;
        let sigma = (fc * ((cents / 1200.0).exp2() - 1.0) / 2.0).max(bin_hz) / bin_hz;
        let centre = fc / bin_hz;
        let lo = ((centre - GAUSS_SPAN * sigma).floor().max(1.0)) as usize;
        let hi = ((centre + GAUSS_SPAN * sigma).ceil() as usize).min(top);
        if hi < lo {
            continue;
        }
        let shape: Vec<f64> = (lo..=hi)
            .map(|i| (-((i as f64 - centre) / sigma).powi(2)).exp())
            .collect();
        let norm: f64 = shape.iter().sum();
        if norm <= 0.0 {
            continue;
        }
        for (p, o) in an.power.iter().zip(&mut out) {
            let lobe = k.saturating_sub(half)..=(k + half).min(p.len() - 1);
            let energy = p[lobe].iter().sum::<f64>() * to_loop;
            for (i, s) in (lo..=hi).zip(&shape) {
                o[i] += energy * s / norm;
            }
        }
    }
    out
}

/// Plain and Ensemble (v0.3 §5.1, §5.3): one inverse FFT of `power` with random phases, the
/// same in every channel, then rotated to the offset whose run-in best matches the original
/// over the entry (circular cross-correlation summed over channels), with equal-power
/// crossfades in and out.
fn random_phase(job: &Job, power: &[Vec<f64>], seed: u64) -> Result<Parts, SpectrumLoopError> {
    let Job { audio, a, l, t, .. } = *job;
    let bins = l / 2 + 1;
    let mut rng = SplitMix64::new(seed);
    let phases: Vec<f32> = (0..bins).map(|_| rng.phase()).collect();
    let fft = Fft::new(l);
    let unit: Vec<Vec<f32>> = power
        .iter()
        .map(|p| {
            let spec: Vec<Complex32> = (0..bins)
                .map(|k| {
                    // No DC, and nothing on the Nyquist bin, which cannot hold a phase.
                    if k == 0 || 2 * k == l {
                        Complex32::default()
                    } else {
                        Complex32::from_polar(p[k].max(0.0).sqrt() as f32, phases[k])
                    }
                })
                .collect();
            fft.inverse(&spec)
        })
        .collect();
    let gain = job.level_gain(&unit).ok_or(SpectrumLoopError::Silent)? as f32;

    // Rotate so the loop's run-in (the frames before its start, played periodically) matches
    // the original over the entry: corr[r] = Σ_c Σ_i x_c[a + i] · loop_c[(i − t + r) mod l].
    let off = l - t % l;
    let mut acc = vec![Complex32::default(); bins];
    for (lp, x) in unit.iter().zip(audio) {
        let mut fold = vec![0.0f32; l];
        for i in 0..t {
            fold[(i + off) % l] += x[a + i];
        }
        let (rf, lf) = (fft.forward(&fold), fft.forward(lp));
        for ((acc, r), y) in acc.iter_mut().zip(&rf).zip(&lf) {
            *acc += r.conj() * y;
        }
    }
    let corr = fft.inverse(&acc);
    let r = (0..l).fold(0, |best, i| if corr[i] > corr[best] { i } else { best });
    let looped: Vec<Vec<f32>> = unit
        .iter()
        .map(|lp| (0..l).map(|i| lp[(i + r) % l] * gain).collect())
        .collect();

    let g = smooth_step(t);
    let entry = looped
        .iter()
        .zip(audio)
        .map(|(lp, x)| {
            (0..t)
                .map(|i| {
                    let (fi, fo) = (g[i] * FRAC_PI_2).sin_cos();
                    (f64::from(x[a + i]) * fo + f64::from(lp[(i + off) % l]) * fi) as f32
                })
                .collect()
        })
        .collect();
    let exit = job.crossfade_exit(&looped);
    Ok(Parts {
        entry,
        looped,
        exit,
    })
}

/// Replaces the selection, or Start…End, with a loop made from its average spectrum, joined
/// by entry and exit transitions (v0.3 §5). See the module docs for the layout.
///
/// - The flavour is Plain when Pitch lock is off or no pitch is found, Steady when the detune
///   is 0, and Ensemble otherwise.
/// - The loop's RMS matches the source range's, lowered if its peak would pass −0.2 dBFS.
/// - The same input and settings always give the same audio.
pub fn render_spectrum_loop(
    audio: &[Vec<f32>],
    e: &EditState,
    selection: Option<(usize, usize)>,
    rate: u32,
    s: &SpectrumLoopSettings,
) -> Result<SpectrumLoopRender, SpectrumLoopError> {
    render_parts(audio, e, selection, rate, s).map(|(r, _)| r)
}

/// `v`, or `default` when it is not finite, clamped to `range`.
fn setting(v: f32, default: f32, range: (f32, f32)) -> f64 {
    f64::from(if v.is_finite() { v } else { default }.clamp(range.0, range.1))
}

/// The flavour of loop to build.
enum Flavour {
    Plain,
    /// The fundamental, its whole number of cycles per loop, and the harmonics used.
    Steady(f64, usize, Vec<usize>),
    /// The fundamental and the detune in cents.
    Ensemble(f64, f64),
}

/// [`render_spectrum_loop`], also handing back the loop exactly as it was generated.
fn render_parts(
    audio: &[Vec<f32>],
    e: &EditState,
    selection: Option<(usize, usize)>,
    rate: u32,
    s: &SpectrumLoopSettings,
) -> Result<(SpectrumLoopRender, Vec<Vec<f32>>), SpectrumLoopError> {
    let an = analyse(audio, e, selection, rate, s.fft_size)?;
    let (a, last) = an.summary.range;
    let b = last + 1;
    let rate_f = f64::from(rate);
    let defaults = SpectrumLoopSettings::default();
    let l = ((setting(s.loop_s, defaults.loop_s, LOOP_S) * rate_f).round() as usize).max(2);
    let t = ((setting(s.transition_ms, defaults.transition_ms, TRANSITION_MS) / 1000.0 * rate_f)
        .round() as usize)
        .min((b - a) / 2);
    let tx = t.min(an.frames - b);
    let detune = setting(s.detune_cents, 0.0, (0.0, MAX_DETUNE_CENTS));

    let mut warnings = Vec::new();
    if an.summary.frames < FEW_FRAMES {
        warnings.push(SpectrumLoopWarning::FewFrames);
    }
    let pitch = an.summary.pitch_hz.map(f64::from);
    let flavour = match pitch.filter(|_| s.pitch_lock) {
        None => Flavour::Plain,
        Some(f0) if detune == 0.0 => {
            let m = ((f0 * l as f64 / rate_f).round() as usize).max(1);
            // Every harmonic up to the Nyquist frequency.
            let hs: Vec<usize> =
                harmonic_peaks(&an.total, an.summary.fft_size, rate_f, f0, usize::MAX)
                    .into_iter()
                    .map(|(h, _)| h)
                    .filter(|&h| 2 * h * m < l)
                    .collect();
            if hs.is_empty() {
                Flavour::Plain
            } else {
                Flavour::Steady(f0, m, hs)
            }
        }
        Some(f0) => {
            if rate_f / l as f64 > f0 * ((detune / 1200.0).exp2() - 1.0) {
                warnings.push(SpectrumLoopWarning::LoopTooShortForDetune);
            }
            Flavour::Ensemble(f0, detune)
        }
    };
    // With Pitch lock on, Plain means no usable pitch: either none was found, or Steady found
    // no harmonic it can place in the loop. The result then reports no pitch either, so the
    // warning and `analysis.pitch_hz` always agree.
    let mut summary = an.summary;
    if s.pitch_lock && matches!(flavour, Flavour::Plain) {
        warnings.push(SpectrumLoopWarning::NoPitch);
        summary.pitch_hz = None;
    }

    let job = Job {
        audio,
        an: &an,
        rate: rate_f,
        a,
        b,
        l,
        t,
        tx,
    };
    let parts = match flavour {
        Flavour::Plain => random_phase(&job, &loop_grid_power(&an, l), s.seed)?,
        Flavour::Steady(f0, m, hs) => steady(&job, f0, m, &hs)?,
        Flavour::Ensemble(f0, cents) => {
            random_phase(&job, &ensemble_power(&an, l, rate_f, f0, cents), s.seed)?
        }
    };

    let spliced: Vec<Vec<f32>> = (0..audio.len())
        .map(|c| {
            [&parts.entry[c], &parts.looped[c], &parts.exit[c]]
                .into_iter()
                .flatten()
                .copied()
                .collect()
        })
        .collect();
    let step = AppliedEffect {
        op: Op::Splice(Arc::new(Splice {
            kind: SpliceKind::SpectrumLoop,
            audio: spliced,
            // The transitions mix in synthetic audio too, so the whole replacement is tinted.
            generated: (0, t + l + tx - 1),
        })),
        first: a,
        last: b + tx - 1,
    };

    // Markers (v0.3 §6): before the range they stay, inside it they move to the loop start,
    // after it they shift by the change in length. End inside the range moves to the end of
    // the exit transition, or to the loop end when there is no audio after the range.
    let loop_start = a + t;
    let loop_end = loop_start + l - 1;
    let place = |f: usize| {
        if f < a {
            f
        } else if f <= last {
            loop_start
        } else {
            (f - (b - a)).saturating_add(t + l)
        }
    };
    let mut edit = e.clone();
    edit.start = place(e.start);
    edit.end = if (a..=last).contains(&e.end) {
        loop_end + tx
    } else {
        place(e.end)
    };
    // The loop must lie inside Start…End. A range wholly outside the trim would leave it
    // outside, so the trim widens to take it in: Start moves back to the loop start, and End
    // forward to the exit end (the loop end when there is no exit). Ranges inside or
    // overlapping the trim are already covered by the moves above and do not change.
    edit.start = edit.start.min(loop_start);
    if edit.end < loop_end {
        edit.end = loop_end + tx;
    }
    edit.loop_mode = LoopMode::Forward;
    edit.loop_start = loop_start;
    edit.loop_end = loop_end;
    edit.crossfade = 0;
    edit.effects.push(step.clone());

    Ok((
        SpectrumLoopRender {
            step,
            edit,
            warnings,
            analysis: summary,
        },
        parts.looped,
    ))
}

#[cfg(test)]
mod tests {

    use std::f64::consts::TAU as TAU64;

    use super::*;
    use crate::dsp::lsq::harmonic_fit;
    use crate::dsp::pitch::character;
    use crate::dsp::rng::SplitMix64;
    use crate::dsp::spectrum::{Fft, welch_power};
    use crate::edit::LoopMode;
    use crate::effects::apply_effect;

    /// The rate of every test signal: low, so the debug-build tests stay quick.
    const RATE: u32 = 22_050;

    /// Frames in `secs` seconds at [`RATE`].
    fn at(secs: f32) -> usize {
        (secs * RATE as f32).round() as usize
    }

    /// A sum of `harmonics` harmonics of `f0`, harmonic `h` with amplitude `1 / h` and phase
    /// `phase · h`, at frame `i` and rate `rate`.
    fn harmonic(i: usize, rate: u32, f0: f32, harmonics: usize, phase: f32) -> f32 {
        let t = i as f64 / f64::from(rate);
        (1..=harmonics)
            .map(|h| {
                let w = std::f64::consts::TAU * f64::from(f0) * h as f64 * t;
                (w + f64::from(phase) * h as f64).cos() as f32 / h as f32
            })
            .sum()
    }

    /// Uniform noise in −1..1.
    fn white(r: &mut SplitMix64) -> f32 {
        r.next_f32() * 2.0 - 1.0
    }

    /// `secs` seconds of a steady mono tone at `f0` with six harmonics.
    fn tone_at(f0: f32, secs: f32) -> Vec<Vec<f32>> {
        vec![
            (0..at(secs))
                .map(|i| 0.3 * harmonic(i, RATE, f0, 6, 0.7))
                .collect(),
        ]
    }

    /// 1.5 s of a steady 220.5 Hz tone (a period of exactly 100 frames).
    fn tone() -> Vec<Vec<f32>> {
        tone_at(220.5, 1.5)
    }

    /// 1.5 s of steady noise, lightly low-passed.
    fn noise() -> Vec<Vec<f32>> {
        let mut r = SplitMix64::new(5);
        let mut z = 0.0f32;
        vec![
            (0..at(1.5))
                .map(|_| {
                    z = 0.6 * z + 0.4 * white(&mut r);
                    0.4 * z
                })
                .collect(),
        ]
    }

    /// 1.5 s of a steady stereo 147 Hz tone, the right channel quieter and 1 radian behind in
    /// every harmonic, with a little independent noise.
    fn stereo_tone() -> Vec<Vec<f32>> {
        let mut r = SplitMix64::new(3);
        let (l, rr) = (0..at(1.5))
            .map(|i| {
                let l = 0.3 * harmonic(i, RATE, 147.0, 6, 0.0) + 0.003 * white(&mut r);
                let rt = 0.2 * harmonic(i, RATE, 147.0, 6, -1.0) + 0.003 * white(&mut r);
                (l, rt)
            })
            .unzip();
        vec![l, rr]
    }

    /// The selection used throughout: 0.25 s to just before 1.25 s, so there is audio before
    /// and after it.
    fn sel() -> (usize, usize) {
        (at(0.25), at(1.25) - 1)
    }

    fn steady(loop_s: f32) -> SpectrumLoopSettings {
        SpectrumLoopSettings {
            loop_s,
            ..Default::default()
        }
    }

    fn ensemble(loop_s: f32, detune_cents: f32) -> SpectrumLoopSettings {
        SpectrumLoopSettings {
            loop_s,
            detune_cents,
            ..Default::default()
        }
    }

    fn plain(loop_s: f32) -> SpectrumLoopSettings {
        SpectrumLoopSettings {
            loop_s,
            pitch_lock: false,
            ..Default::default()
        }
    }

    /// Renders over the selection and returns the result, the generated loop and the audio
    /// after the step, checking what every render must satisfy.
    fn run(
        x: &[Vec<f32>],
        e: &EditState,
        selection: Option<(usize, usize)>,
        s: &SpectrumLoopSettings,
    ) -> (SpectrumLoopRender, Vec<Vec<f32>>, Vec<Vec<f32>>) {
        let (r, looped) = render_parts(x, e, selection, RATE, s).expect("renders");
        let mut out = x.to_vec();
        apply_effect(&mut out, &r.step);
        assert_eq!(r.edit.effects.last(), Some(&r.step));
        assert_eq!(r.edit.loop_mode, LoopMode::Forward);
        assert_eq!(r.edit.crossfade, 0);
        assert_eq!(r.edit.loop_len(), looped[0].len());
        assert!(out.iter().flatten().all(|v| v.is_finite()), "finite");
        check_trim(&r, x[0].len());
        check_pitch_agrees(&r, s);
        (r, looped, out)
    }

    fn run_sel(
        x: &[Vec<f32>],
        s: &SpectrumLoopSettings,
    ) -> (SpectrumLoopRender, Vec<Vec<f32>>, Vec<Vec<f32>>) {
        run(x, &EditState::default_for(x[0].len()), Some(sel()), s)
    }

    /// The largest step from frame `i` to `i + 1` over the channels.
    fn step(x: &[Vec<f32>], i: usize) -> f32 {
        x.iter()
            .map(|c| (c[i + 1] - c[i]).abs())
            .fold(0.0, f32::max)
    }

    /// The 99th percentile of the steps inside `first..=last`, over all channels.
    fn p99_step(x: &[Vec<f32>], first: usize, last: usize) -> f32 {
        let mut d: Vec<f32> = x
            .iter()
            .flat_map(|c| (first..last).map(move |i| (c[i + 1] - c[i]).abs()))
            .collect();
        d.sort_by(f32::total_cmp);
        d[(d.len() as f32 * 0.99) as usize]
    }

    /// The largest step within three frames either side of the boundary before frame `at`.
    fn step_near(x: &[Vec<f32>], at: usize) -> f32 {
        (at - 3..at + 3).map(|i| step(x, i)).fold(0.0, f32::max)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn db(v: f32) -> f32 {
        20.0 * v.log10()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn loop_is_exactly_periodic() {
        let x = tone();
        for (name, s) in [
            ("steady", steady(1.0)),
            ("ensemble", ensemble(1.0, 20.0)),
            ("plain", plain(1.0)),
        ] {
            let (r, _, out) = run_sel(&x, &s);
            for ch in &out {
                let lp = &ch[r.edit.loop_start..=r.edit.loop_end];
                let twice: Vec<f32> = lp.iter().chain(lp).copied().collect();
                let n = lp.len();
                assert_eq!(&twice[..n], &twice[n..], "{name}: identical halves");
                let inside = (0..n - 1)
                    .map(|i| (lp[i + 1] - lp[i]).abs())
                    .fold(0.0, f32::max);
                let wrap = (twice[n] - twice[n - 1]).abs();
                assert!(wrap <= inside, "{name}: wrap {wrap} vs inside {inside}");
            }
        }
    }

    #[test]
    fn steady_keeps_pitch_level_and_shape() {
        let x = tone();
        let (a, last) = sel();
        let (r, looped, _) = run_sel(&x, &steady(2.0));
        let lp = &looped[0];
        let c = character(&looped, 0, lp.len() - 1, RATE, 0.45);
        let hz = RATE as f32 / c.period.expect("pitched");
        let cents = 1200.0 * (hz / 220.5).log2();
        assert!(cents.abs() <= 5.0, "pitch {hz} Hz, {cents} cents");

        let src_rms = rms(&x[0][a..=last]);
        let w = at(0.05);
        for (i, win) in lp.chunks_exact(w).enumerate() {
            let d = db(rms(win) / src_rms);
            assert!(d.abs() <= 1.0, "50 ms RMS {i}: {d} dB");
        }

        let period = (RATE as f32 / hz).round() as usize;
        for k in 0..lp.len() / period - 1 {
            let p = &lp[k * period..(k + 1) * period];
            let q = &lp[(k + 1) * period..(k + 2) * period];
            let num: f32 = p.iter().zip(q).map(|(u, v)| u * v).sum();
            let den = (p.iter().map(|u| u * u).sum::<f32>() * q.iter().map(|v| v * v).sum::<f32>())
                .sqrt();
            assert!(num / den >= 0.99, "period {k}: {}", num / den);
        }

        let d = db(peak(lp) / peak(&x[0][a..=last]));
        assert!(d.abs() <= 3.0, "peak {d} dB");
        assert_eq!(r.analysis.range, (a, last));
    }

    /// Band power in third-octave bands from 200 Hz to 8 kHz, as a share of the total, in dB,
    /// from a power spectrum whose bin `k` is at `k · bin_hz`.
    fn third_octaves(p: &[f32], bin_hz: f32) -> Vec<f32> {
        let total: f32 = p.iter().sum();
        let mut out = Vec::new();
        let mut fc = 200.0f32;
        while fc <= 8000.0 {
            let (lo, hi) = (fc * 2f32.powf(-1.0 / 6.0), fc * 2f32.powf(1.0 / 6.0));
            let band: f32 = p
                .iter()
                .enumerate()
                .filter(|(k, _)| (lo..hi).contains(&(*k as f32 * bin_hz)))
                .map(|(_, v)| v)
                .sum();
            out.push(10.0 * (band / total).log10());
            fc *= 2f32.powf(1.0 / 3.0);
        }
        out
    }

    #[test]
    fn plain_matches_the_average_spectrum() {
        let x = noise();
        let (a, last) = sel();
        let (r, looped, _) = run_sel(&x, &plain(1.0));
        let n = r.analysis.fft_size;
        let src = third_octaves(&welch_power(&x[0], a, last, n), r.analysis.bin_hz);
        // The loop is periodic, so two copies give its average spectrum over every offset.
        let lp: Vec<f32> = looped[0].iter().chain(&looped[0]).copied().collect();
        let got = third_octaves(&welch_power(&lp, 0, lp.len() - 1, n), r.analysis.bin_hz);
        for (i, (s, g)) in src.iter().zip(&got).enumerate() {
            assert!((s - g).abs() <= 2.0, "band {i}: source {s} dB, loop {g} dB");
        }
        // Same level overall.
        let d = db(rms(&looped[0]) / rms(&x[0][a..=last]));
        assert!(d.abs() < 0.1, "level {d} dB");
    }

    /// The −6 dB width in Hz of the loop's spectral peak nearest `hz`.
    fn width_6db(lp: &[f32], hz: f32) -> f32 {
        let spec = Fft::new(lp.len()).forward(lp);
        let bin = RATE as f32 / lp.len() as f32;
        let c = (hz / bin).round() as usize;
        let span = (20.0 / bin) as usize;
        let k = (c - span..=c + span)
            .max_by(|&i, &j| spec[i].norm().total_cmp(&spec[j].norm()))
            .unwrap();
        let half = spec[k].norm() / 2.0;
        let lo = (0..k).rev().find(|&i| spec[i].norm() < half).unwrap();
        let hi = (k..spec.len()).find(|&i| spec[i].norm() < half).unwrap();
        (hi - lo - 1) as f32 * bin
    }

    #[test]
    fn ensemble_widens_with_detune() {
        let x = tone();
        let width = |cents| {
            let (_, looped, _) = run_sel(&x, &ensemble(1.0, cents));
            width_6db(&looped[0], 4.0 * 220.5)
        };
        let (w10, w30) = (width(10.0), width(30.0));
        assert!(w30 >= 2.0 * w10, "10 cents {w10} Hz, 30 cents {w30} Hz");
    }

    #[test]
    fn transitions_are_click_free_and_leave_the_loop_alone() {
        let cases = [
            ("tone, steady", tone(), steady(1.0)),
            ("noise, plain", noise(), plain(1.0)),
            ("stereo tone, steady", stereo_tone(), steady(1.0)),
            ("tone, ensemble", tone(), ensemble(1.0, 10.0)),
            ("tone, plain", tone(), plain(1.0)),
            ("stereo tone, ensemble", stereo_tone(), ensemble(1.0, 20.0)),
            ("stereo tone, plain", stereo_tone(), plain(1.0)),
        ];
        let (a, last) = sel();
        let b = last + 1;
        for (name, x, s) in cases {
            let (r, looped, out) = run_sel(&x, &s);
            let t = r.edit.loop_start - a;
            let l = r.edit.loop_len();
            let tx = r.step.last + 1 - b;
            assert_eq!(t, at(0.1), "{name}: entry length");
            assert_eq!(tx, at(0.1), "{name}: exit length");
            for (c, ch) in out.iter().enumerate() {
                assert_eq!(
                    &ch[r.edit.loop_start..=r.edit.loop_end],
                    &looped[c][..],
                    "{name}: the loop is untouched"
                );
                assert_eq!(&ch[..a], &x[c][..a], "{name}: before");
                assert_eq!(&ch[a + t + l + tx..], &x[c][b + tx..], "{name}: after");
            }
            let limit = 1.5 * p99_step(&x, a, last);
            for (what, i) in [
                ("entry start", a),
                ("loop start", a + t),
                ("loop end", a + t + l),
                ("exit end", a + t + l + tx),
            ] {
                let st = step_near(&out, i);
                assert!(st <= limit, "{name}: {what} step {st} > {limit}");
            }
            if s.pitch_lock && s.detune_cents == 0.0 {
                for c in 0..x.len() {
                    let d = (out[c][a] - x[c][a]).abs();
                    assert!(d <= 0.01, "{name}: first entry frame off by {d}");
                }
            }
        }
    }

    #[test]
    fn steady_exit_near_the_end_is_a_crossfade() {
        // 30 ms after the range: shorter than Steady's 50 ms fit window, so the exit is the
        // Plain crossfade, as long as the audio after the range.
        for x in [tone(), stereo_tone()] {
            let frames = x[0].len();
            let (a, last) = (at(0.25), frames - at(0.03) - 1);
            let e = EditState::default_for(frames);
            let (r, looped, out) = run(&x, &e, Some((a, last)), &steady(1.0));
            let (t, l, tx) = (r.edit.loop_start - a, r.edit.loop_len(), at(0.03));
            assert_eq!(
                r.step.last,
                frames - 1,
                "the exit runs to the end of the file"
            );
            assert_eq!(out[0].len(), a + t + l + tx);
            assert_eq!(r.edit.end, a + t + l + tx - 1, "End on the exit end");
            let limit = 1.5 * p99_step(&x, a, last);
            for i in [a, a + t, a + t + l] {
                assert!(step_near(&out, i) <= limit, "step at {i}");
            }
            for c in 0..x.len() {
                assert_eq!(&out[c][a + t..a + t + l], &looped[c][..]);
                let d = (out[c][frames + t + l - (last + 1 - a) - 1] - x[c][frames - 1]).abs();
                assert!(d < 0.01, "lands on the original's last frame, off by {d}");
            }
        }
    }

    /// The fundamental's frequency, once per period of `f_ref` from `first` to `last`, from the
    /// phase advance over that period of a six-harmonic fit at `f_ref` over two periods. A hop
    /// of one period makes the fit's errors from a slightly wrong `f_ref` cancel.
    fn fundamental_track(x: &[f32], first: usize, last: usize, f_ref: f64) -> Vec<f64> {
        let hop = (f64::from(RATE) / f_ref).round() as usize;
        let w = 2 * hop;
        let freqs: Vec<f64> = (1..=6).map(|h| h as f64 * f_ref).collect();
        let phase = |s: usize| harmonic_fit(&x[s..s + w], &freqs, RATE)[0].arg();
        let mut out = Vec::new();
        let mut s = first;
        while s + hop <= last {
            let expected = std::f64::consts::TAU * f_ref * hop as f64 / f64::from(RATE);
            let d = phase(s + hop) - phase(s) - expected;
            let d =
                (d + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI;
            out.push(f_ref + d * f64::from(RATE) / (std::f64::consts::TAU * hop as f64));
            s += hop;
        }
        out
    }

    #[test]
    fn steady_entry_glides_smoothly() {
        // A 0.1 s loop rounds 97 Hz to 100 Hz, so the entry has a real glide to make.
        let x = tone_at(97.0, 1.5);
        let (a, _) = sel();
        let (r, _, out) = run_sel(&x, &steady(0.1));
        let w = at(0.05);
        // Windows that start from 50 ms before the entry until the loop has taken over.
        let track = fundamental_track(&out[0], a - w, r.edit.loop_start + 1, 98.5);
        for p in track.windows(2) {
            assert!((p[1] - p[0]).abs() <= 1.0, "jump {} -> {}", p[0], p[1]);
        }
        let (first, last) = (track[0], *track.last().unwrap());
        assert!((first - 97.0).abs() < 0.5, "starts at {first}");
        assert!((last - 100.0).abs() < 0.5, "ends at {last}");
    }

    #[test]
    fn markers_follow_the_new_length() {
        let x = tone();
        let frames = x[0].len();
        let (a, last) = sel();
        let b = last + 1;
        let s = steady(1.0);
        let (t, l) = (at(0.1), at(1.0));
        let grow = |f: usize| f + t + l - (b - a);

        // Start before the range stays; End after it shifts.
        let mut e = EditState::default_for(frames);
        e.start = 1000;
        e.end = frames - 2000;
        let (r, _, out) = run(&x, &e, Some(sel()), &s);
        assert_eq!(out[0].len(), grow(frames));
        assert_eq!(r.edit.start, 1000);
        assert_eq!(r.edit.end, grow(frames - 2000));
        assert_eq!((r.edit.loop_start, r.edit.loop_end), (a + t, a + t + l - 1));

        // Start inside the range moves to the loop start.
        e.start = a + 50;
        let (r, _, _) = run(&x, &e, Some(sel()), &s);
        assert_eq!(r.edit.start, a + t);

        // The range is Start…End: End moves to the end of the exit transition.
        let mut e = EditState::default_for(frames);
        (e.start, e.end) = sel();
        let (r, _, out) = run(&x, &e, None, &s);
        assert_eq!(r.analysis.range, sel());
        assert_eq!(r.edit.start, a + t);
        assert_eq!(r.edit.end, a + t + l + t - 1);
        assert_eq!(out[0].len(), grow(frames));

        // Start…End reaching the end of the file: no exit, End on the loop end.
        e.end = frames - 1;
        let (r, _, out) = run(&x, &e, None, &s);
        assert_eq!(r.edit.end, r.edit.loop_end);
        assert_eq!(r.step.last, frames - 1);
        assert_eq!(out[0].len(), r.edit.loop_end + 1);
    }

    /// The returned edit keeps the loop inside Start…End, so clamping it changes nothing about
    /// the loop.
    fn check_trim(r: &SpectrumLoopRender, original_frames: usize) {
        let e = &r.edit;
        assert!(
            e.start <= e.loop_start && e.loop_start < e.loop_end && e.loop_end <= e.end,
            "start {} loop {}..={} end {}",
            e.start,
            e.loop_start,
            e.loop_end,
            e.end
        );
        let c = e.clone().clamped(original_frames);
        assert_eq!(
            (c.loop_mode, c.loop_start, c.loop_end, c.crossfade),
            (e.loop_mode, e.loop_start, e.loop_end, e.crossfade),
            "clamping leaves the loop alone"
        );
    }

    /// With Pitch lock on, the `NoPitch` warning and a missing pitch go together.
    fn check_pitch_agrees(r: &SpectrumLoopRender, s: &SpectrumLoopSettings) {
        if s.pitch_lock {
            assert_eq!(
                r.warnings.contains(&SpectrumLoopWarning::NoPitch),
                r.analysis.pitch_hz.is_none(),
                "{:?} with pitch {:?}",
                r.warnings,
                r.analysis.pitch_hz
            );
        }
    }

    #[test]
    fn a_range_outside_the_trim_widens_it() {
        let x = tone();
        let frames = x[0].len();
        let (a, last) = sel();
        let b = last + 1;
        let (t, l) = (at(0.1), at(1.0));
        let s = steady(1.0);

        // The selection lies wholly after End: End moves to the exit end.
        let mut e = EditState::default_for(frames);
        e.end = a - 100;
        let (r, _, _) = run(&x, &e, Some(sel()), &s);
        assert_eq!(r.edit.start, 0);
        assert_eq!(r.edit.end, a + t + l + t - 1);

        // The selection lies wholly before Start: Start moves back to the loop start, and End
        // shifts with the audio after the range.
        let mut e = EditState::default_for(frames);
        e.start = b + 500;
        let (r, _, _) = run(&x, &e, Some(sel()), &s);
        assert_eq!(r.edit.start, a + t);
        assert_eq!(r.edit.end, frames - 1 + t + l - (b - a));

        // Wholly after End with no audio after the range: End moves to the loop end.
        let mut e = EditState::default_for(frames);
        e.end = a - 100;
        let (r, _, _) = run(&x, &e, Some((a, frames - 1)), &s);
        assert_eq!(r.edit.end, r.edit.loop_end);
        assert_eq!(r.step.last, frames - 1);
    }

    #[test]
    fn steady_keeps_every_harmonic_up_to_nyquist() {
        // A low, bright 50 Hz tone with 219 harmonics, the highest at 10.95 kHz.
        let f0 = 50.0f32;
        let x = vec![
            (0..at(1.5))
                .map(|i| {
                    let t = f64::from(i as u32) / f64::from(RATE);
                    let v: f64 = (1..=219)
                        .map(|h| {
                            let w = std::f64::consts::TAU * f64::from(f0) * f64::from(h) * t;
                            (w + 0.37 * f64::from(h * h)).cos() / f64::from(h).sqrt()
                        })
                        .sum();
                    (0.02 * v) as f32
                })
                .collect::<Vec<f32>>(),
        ];
        let (a, last) = sel();
        let (r, looped, _) = run_sel(&x, &steady(1.0));
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let n = r.analysis.fft_size;
        let above = |p: Vec<f32>| -> f32 {
            p.iter()
                .enumerate()
                .filter(|(k, _)| *k as f32 * r.analysis.bin_hz > 60.0 * f0 + f0 / 2.0)
                .map(|(_, v)| v)
                .sum()
        };
        let src = above(welch_power(&x[0], a, last, n));
        let lp: Vec<f32> = looped[0].iter().chain(&looped[0]).copied().collect();
        let got = above(welch_power(&lp, 0, lp.len() - 1, n));
        let d = 10.0 * (got / src).log10();
        assert!(d.abs() <= 3.0, "energy above the 60th harmonic: {d} dB");
        // The whole spectrum keeps its level too.
        let d = db(rms(&looped[0]) / rms(&x[0][a..=last]));
        assert!(d.abs() <= 0.5, "level {d} dB");
    }

    #[test]
    fn steady_without_a_usable_harmonic_reports_no_pitch() {
        // At 4 kHz a 1379 Hz tone is found as the pitch, but its first harmonic already lies
        // within half a fundamental of the Nyquist frequency, so Steady has nothing to place
        // and falls back to Plain.
        let rate = 4000;
        let x = vec![
            (0..rate as usize)
                .map(|i| 0.3 * (TAU64 * 1379.0 * i as f64 / f64::from(rate)).sin() as f32)
                .collect::<Vec<f32>>(),
        ];
        let e = EditState::default_for(x[0].len());
        let s = steady(1.0);
        let found = analyse_range(&x, &e, None, rate, None).expect("analyses");
        assert!(found.pitch_hz.is_some(), "the analysis finds a pitch");
        let r = render_spectrum_loop(&x, &e, None, rate, &s).expect("renders");
        assert_eq!(r.warnings, vec![SpectrumLoopWarning::NoPitch]);
        assert_eq!(r.analysis.pitch_hz, None);
        check_pitch_agrees(&r, &s);
        let plain = render_spectrum_loop(&x, &e, None, rate, &plain(1.0)).expect("renders");
        assert_eq!(r.step, plain.step, "it is Plain");
    }

    #[test]
    fn spectrum_loop_of_silence_is_refused() {
        let x = vec![vec![0.0f32; at(1.5)]];
        let e = EditState::default_for(x[0].len());
        assert_eq!(
            render_spectrum_loop(&x, &e, Some(sel()), RATE, &steady(1.0)),
            Err(SpectrumLoopError::Silent)
        );
        assert_eq!(
            analyse_range(&x, &e, None, RATE, None),
            Err(SpectrumLoopError::Silent)
        );
        // Very quiet audio still works, with nothing but finite samples.
        let quiet: Vec<Vec<f32>> = vec![tone()[0].iter().map(|v| v * 1e-6).collect()];
        for s in [steady(1.0), ensemble(1.0, 20.0), plain(1.0)] {
            run_sel(&quiet, &s);
        }
        // Nothing to work on.
        assert_eq!(
            analyse_range(&[], &e, None, RATE, None),
            Err(SpectrumLoopError::NoAudio)
        );
        assert_eq!(
            analyse_range(&tone(), &e, Some((100, 200)), RATE, None),
            Err(SpectrumLoopError::RangeTooShort)
        );
    }

    #[test]
    fn auto_fft_size_and_warnings() {
        let rate = 11_025;
        let x = vec![
            (0..rate as usize / 2)
                .map(|i| 0.3 * harmonic(i, rate, 220.0, 6, 0.7))
                .collect::<Vec<f32>>(),
        ];
        let e = EditState::default_for(x[0].len());
        let range = (0, (0.35 * rate as f32) as usize - 1);
        let an = analyse_range(&x, &e, Some(range), rate, None).expect("analyses");
        assert_eq!(an.fft_size, 2048);
        assert_eq!(an.range, range);
        assert!((an.bin_hz - rate as f32 / 2048.0).abs() < 1e-6);
        let hz = an.pitch_hz.expect("pitched");
        assert!((hz - 220.0).abs() < 1.0, "{hz} Hz");

        let x = tone_at(97.0, 1.5);
        let r = run_sel(&x, &ensemble(1.0, 10.0)).0;
        assert!(
            r.warnings
                .contains(&SpectrumLoopWarning::LoopTooShortForDetune)
        );
        let r = run_sel(&x, &ensemble(3.0, 10.0)).0;
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);

        let x = noise();
        let (r, _, out) = run_sel(&x, &steady(1.0));
        assert_eq!(r.warnings, vec![SpectrumLoopWarning::NoPitch]);
        assert_eq!(r.analysis.pitch_hz, None);
        let (_, _, out_plain) = run_sel(&x, &plain(1.0));
        assert_eq!(out, out_plain, "noise with Pitch lock on is Plain");

        // A big FFT on a short range: few frames.
        let r = run_sel(
            &tone(),
            &SpectrumLoopSettings {
                fft_size: Some(16_384),
                ..steady(1.0)
            },
        )
        .0;
        assert_eq!(r.analysis.fft_size, 16_384);
        assert!(r.analysis.frames < 3);
        assert!(r.warnings.contains(&SpectrumLoopWarning::FewFrames));
    }

    #[test]
    fn same_seed_same_audio_new_seed_differs() {
        let x = stereo_tone();
        for s in [plain(1.0), ensemble(1.0, 20.0)] {
            let (_, _, one) = run_sel(&x, &s);
            let (_, _, again) = run_sel(&x, &s);
            assert_eq!(one, again);
            let (_, _, other) = run_sel(&x, &SpectrumLoopSettings { seed: 2, ..s });
            assert_ne!(one, other);
        }
    }

    #[test]
    fn odd_settings_never_panic() {
        let x = stereo_tone();
        for s in [
            SpectrumLoopSettings {
                fft_size: Some(3),
                detune_cents: f32::NAN,
                loop_s: f32::NAN,
                transition_ms: f32::INFINITY,
                ..Default::default()
            },
            SpectrumLoopSettings {
                fft_size: Some(1 << 20),
                detune_cents: 500.0,
                loop_s: 1e9,
                transition_ms: -5.0,
                ..Default::default()
            },
        ] {
            let e = EditState::default_for(x[0].len());
            let r = render_spectrum_loop(&x, &e, Some((usize::MAX, 3000)), RATE, &s);
            assert!(r.is_ok());
        }
    }
}
