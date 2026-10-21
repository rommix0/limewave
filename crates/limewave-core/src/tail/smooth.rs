//! The Smooth method (v0.3 §4.4): freezes the spectrum at the cut and lets each frequency fade
//! at its own rate.
//!
//! - **Freeze**: the magnitudes are the mean of the last two spectral frames, the phases are
//!   the last frame's, and each bin's true frequency comes from the phase change between the
//!   two (the phase-vocoder estimate).
//! - **Peak locking**: the bins within 2 of a prominent peak advance at that peak's frequency,
//!   so each partial stays one coherent tone. For a tonal source every peak is also snapped to
//!   a harmonic of one refined fundamental, so the harmonics stay in step and the wave keeps
//!   its shape.
//! - **DC**: locked like a peak, so DC and the bins of its main lobe that no peak claims keep
//!   their phase and never turn; a DC offset stays a steady offset.
//! - **Noise**: every other bin gets a fresh seeded random phase each frame, with the same
//!   offset in every channel, which keeps the stereo relationship.
//! - **Synthesis**: overlap-add of Hann frames at the analysis hop, normalized by the summed
//!   squared window. Frame 0 is the last analysis frame, which ends exactly at the cut.
//! - **Level**: each bin is carried from the frozen frame's centre to the cut at its measured
//!   rate, then fades at its target rate (Decay) or holds (the Sustain body).
//! - **Sustain** (§4.8): the body is exactly periodic in L, a whole number of hops. Every
//!   frequency completes whole cycles in L, and the noise phases repeat every L / hop frames.
//!   The release continues the periodic signal, and only frames starting after the loop end
//!   fade, timed from the loop end to their centres as in Decay, so the loop's own audio is
//!   never changed.
//!
//! The analysis, the synthesis and the join are `pub(crate)` so the Harmonic method can reuse
//! them for its partials, its rates and its noise layer.

use std::f64::consts::{PI, TAU};

use super::{
    Generated, Resolved, STEADY_DECAY_T60, TailAnalysis, TailError, TailMode, TailWarning,
    end_ramp, join_overlap, sustain_lengths, tail_length,
};
use crate::dsp::envelope::DecayFit;
use crate::dsp::rng::SplitMix64;
use crate::dsp::spectrum::{Complex32, Fft, frame_size_for, frames_ending_at, hann};
use crate::edit::EditState;

/// Spectral frames are about this many seconds, rounded down to a power of two...
const FRAME_S: f32 = 0.093;

/// ...and small enough that this many fit in the source region.
const MIN_FRAMES: f32 = 2.5;

/// Per-bin rates are fitted over this many of the most recent frames...
const RATE_FRAMES: usize = 8;

/// ...and smoothed over this many bins either side (9 bins in all)...
const RATE_SMOOTH_HALF: usize = 4;

/// ...and, for a decaying source, kept within these multiples of the overall rate.
const RATE_LIMITS: (f32, f32) = (0.5, 4.0);

/// A peak's local median is taken over this many bins either side.
const MEDIAN_HALF: usize = 8;

/// A peak stands more than this far above its local median, in dB...
const PEAK_ABOVE_MEDIAN_DB: f32 = 12.0;

/// ...and within this range of the loudest bin.
const PEAK_RANGE_DB: f32 = 70.0;

/// Bins this close to a peak are locked to it.
const LOCK_BINS: usize = 2;

/// At full Darken the rate at the Nyquist frequency is multiplied by 1 + this.
const DARKEN_GAIN: f32 = 3.0;

/// The tail lasts until the slowest bin within this range of the loudest has faded out.
const LENGTH_RANGE_DB: f32 = 40.0;

/// Keeps a logarithm finite for a silent bin (an energy, in unnormalized FFT units).
const TINY_ENERGY: f64 = 1e-30;

/// How the level of every bin moves over the synthesized frames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Course {
    /// Carried from the frozen frame's centre to the cut at the measured rates, then fading at
    /// the target rates.
    Decay,
    /// Held at the level carried to the cut, and periodic in `loop_len` frames (a whole number
    /// of hops) from the cut. With `release`, frames that start after the loop end fade at the
    /// target rates from there.
    Sustain { loop_len: usize, release: bool },
}

/// Spectral frames laid out `[channel][frame][bin]`, oldest frame first.
pub(crate) type Frames = Vec<Vec<Vec<Complex32>>>;

/// A frozen spectrum, ready to synthesize.
pub(crate) struct Frozen {
    /// The frame size (a power of two).
    pub(crate) n: usize,
    /// The hop, a quarter frame.
    pub(crate) hop: usize,
    /// Per channel and bin: the frozen magnitude.
    pub(crate) mag: Vec<Vec<f32>>,
    /// Per channel and bin: the phase of the last analysis frame, as a unit phasor.
    pub(crate) phasor: Vec<Vec<Complex32>>,
    /// Per bin: the frequency a tonal bin's phase advances at, in radians per sample.
    pub(crate) omega: Vec<f64>,
    /// Per bin: true when the bin takes a fresh random phase every frame instead.
    pub(crate) noisy: Vec<bool>,
    /// Per bin, in dB per second: the measured fall, which carries the level up to the cut.
    pub(crate) measured: Vec<f32>,
    /// Per bin, in dB per second: the target fall after the cut.
    pub(crate) target: Vec<f32>,
}

/// The analysis frames of every channel for the spectral methods (v0.3 §4.2): Hann frames of
/// about 93 ms (a power of two, small enough that 2.5 frames fit in `first..cut`) at a hop of
/// a quarter frame, the last ending exactly at `cut` (exclusive) and none starting before
/// `first`.
///
/// Returns the frame size and the frames laid out `[channel][frame][bin]`, oldest frame first;
/// every channel has the same number of frames. `SourceTooShort` when fewer than two fit.
pub(crate) fn spectral_frames(
    audio: &[Vec<f32>],
    first: usize,
    cut: usize,
    rate: u32,
) -> Result<(usize, Frames), TailError> {
    let n = frame_size_for(cut.saturating_sub(first), rate, FRAME_S, MIN_FRAMES);
    if n < 4 {
        return Err(TailError::SourceTooShort);
    }
    let frames: Frames = audio
        .iter()
        .map(|ch| frames_ending_at(ch, cut, n, first))
        .collect();
    if frames.iter().map(Vec::len).min().unwrap_or(0) < 2 {
        return Err(TailError::SourceTooShort);
    }
    Ok((n, frames))
}

/// The energy of bin `k` of frame `j`, summed over channels.
fn energy(frames: &[Vec<Vec<Complex32>>], j: usize, k: usize) -> f64 {
    frames.iter().map(|ch| f64::from(ch[j][k].norm_sqr())).sum()
}

/// The per-bin decay rates in dB per second (v0.3 §4.2, §4.7), as `(measured, target)`.
///
/// `frames` is laid out `[channel][frame][bin]`, oldest frame first, at a hop of `hop` frames
/// at `rate` (as [`spectral_frames`] gives it). Channels are combined by energy.
///
/// - Each bin's slope is a straight-line fit of its dB level over the last 8 frames (with
///   fewer than 3 frames, the overall slope), averaged over the 9 neighbouring bins (fewer at
///   the edges of the spectrum).
/// - **Measured**: the fall that carries a frozen frame's level up to the cut. For a decaying
///   source it is limited to 0–4× the overall rate; a steady source has none.
/// - **Target**: the fall after the cut. For a decaying source it is limited to 0.5×–4× the
///   overall rate, and a `settings_t60` (seconds to fall 60 dB) scales every bin together; for
///   a steady source every bin falls 60 dB in `settings_t60`, or in 1.0 s when that is `None`.
///   A `settings_t60` that is not a positive number counts as `None`.
/// - **Darken** (0–1) multiplies the target by up to 1 + 3·Darken at the Nyquist frequency,
///   rising linearly with frequency.
pub(crate) fn per_bin_rates(
    frames: &[Vec<Vec<Complex32>>],
    fit: &DecayFit,
    settings_t60: Option<f32>,
    darken: f32,
    rate: u32,
    hop: usize,
) -> (Vec<f32>, Vec<f32>) {
    let bins = frames.first().and_then(|ch| ch.first()).map_or(0, Vec::len);
    let count = frames.iter().map(Vec::len).min().unwrap_or(0);
    let user_t60 = settings_t60.filter(|t| t.is_finite() && *t > 0.0);
    let overall = fit.rate_db_s;
    let decaying = !fit.steady && overall > 0.0;

    // Each bin's fall in dB per second (positive when falling), over the last frames.
    let used = count.min(RATE_FRAMES);
    let fall: Vec<f64> = if used >= 3 {
        let secs = hop as f64 / f64::from(rate.max(1));
        let t: Vec<f64> = (0..used).map(|i| i as f64 * secs).collect();
        let tm = t.iter().sum::<f64>() / used as f64;
        let var: f64 = t.iter().map(|v| (v - tm) * (v - tm)).sum();
        (0..bins)
            .map(|k| {
                let db: Vec<f64> = (count - used..count)
                    .map(|j| 10.0 * energy(frames, j, k).max(TINY_ENERGY).log10())
                    .collect();
                let dm = db.iter().sum::<f64>() / used as f64;
                let cov: f64 = t.iter().zip(&db).map(|(a, b)| (a - tm) * (b - dm)).sum();
                -cov / var
            })
            .collect()
    } else {
        vec![f64::from(overall); bins]
    };
    let fall: Vec<f32> = (0..bins)
        .map(|k| {
            let lo = k.saturating_sub(RATE_SMOOTH_HALF);
            let hi = (k + RATE_SMOOTH_HALF).min(bins - 1);
            (fall[lo..=hi].iter().sum::<f64>() / (hi + 1 - lo) as f64) as f32
        })
        .collect();

    let (measured, mut target): (Vec<f32>, Vec<f32>) = if decaying {
        let scale = user_t60.map_or(1.0, |t| 60.0 / t / overall);
        let (lo, hi) = (RATE_LIMITS.0 * overall, RATE_LIMITS.1 * overall);
        fall.iter()
            .map(|&f| (f.clamp(0.0, hi), f.clamp(lo, hi) * scale))
            .unzip()
    } else {
        let g = 60.0 / user_t60.unwrap_or(STEADY_DECAY_T60);
        (vec![0.0; bins], vec![g; bins])
    };
    let darken = if darken.is_finite() {
        darken.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let top = bins.saturating_sub(1).max(1) as f32;
    for (k, r) in target.iter_mut().enumerate() {
        *r *= 1.0 + DARKEN_GAIN * darken * k as f32 / top;
    }
    (measured, target)
}

/// The prominent peaks of a magnitude spectrum (v0.3 §4.4): local maxima more than 12 dB above
/// the median of the 17 bins around them and within 70 dB of the loudest bin. DC and the
/// Nyquist bin are never peaks. In ascending bin order.
pub(crate) fn peak_bins(mag: &[f32]) -> Vec<usize> {
    let db: Vec<f32> = mag
        .iter()
        .map(|&m| (10.0 * (f64::from(m).powi(2)).max(TINY_ENERGY).log10()) as f32)
        .collect();
    let loudest = db.iter().copied().fold(f32::MIN, f32::max);
    let mut window = Vec::with_capacity(2 * MEDIAN_HALF + 1);
    (1..db.len().saturating_sub(1))
        .filter(|&i| {
            if db[i] < db[i - 1] || db[i] < db[i + 1] || db[i] <= loudest - PEAK_RANGE_DB {
                return false;
            }
            window.clear();
            window.extend_from_slice(
                &db[i.saturating_sub(MEDIAN_HALF)..(i + MEDIAN_HALF + 1).min(db.len())],
            );
            window.sort_unstable_by(f32::total_cmp);
            db[i] - window[window.len() / 2] > PEAK_ABOVE_MEDIAN_DB
        })
        .collect()
}

/// Whether the DC bin of a magnitude spectrum stands out as a peak would ([`peak_bins`]): at
/// least as loud as bin 1, more than 12 dB above the median of the bins up to 8 above it, and
/// within 70 dB of the loudest bin.
fn dc_stands_out(mag: &[f32]) -> bool {
    let db = |m: f32| (10.0 * (f64::from(m).powi(2)).max(TINY_ENERGY).log10()) as f32;
    let Some(&dc) = mag.first() else {
        return false;
    };
    let loudest = mag.iter().copied().fold(0.0f32, f32::max);
    let mut window: Vec<f32> = mag.iter().take(MEDIAN_HALF + 1).map(|&m| db(m)).collect();
    window.sort_unstable_by(f32::total_cmp);
    let median = window[window.len() / 2];
    mag.get(1).is_none_or(|&m| dc >= m)
        && db(dc) - median > PEAK_ABOVE_MEDIAN_DB
        && db(dc) > db(loudest) - PEAK_RANGE_DB
}

/// For each bin, the peak it is locked to: the nearest peak within 2 bins (the louder on a
/// tie), or `None` for a noise bin. Every peak is locked to itself.
pub(crate) fn lock_bins(peaks: &[usize], mag: &[f32]) -> Vec<Option<usize>> {
    let mut lock: Vec<Option<usize>> = vec![None; mag.len()];
    for &p in peaks {
        let lo = p.saturating_sub(LOCK_BINS);
        let hi = (p + LOCK_BINS).min(mag.len().saturating_sub(1));
        for (i, slot) in lock.iter_mut().enumerate().take(hi + 1).skip(lo) {
            let better = match *slot {
                None => true,
                Some(q) => {
                    let (dp, dq) = (i.abs_diff(p), i.abs_diff(q));
                    dp < dq || (dp == dq && mag[p] > mag[q])
                }
            };
            if better {
                *slot = Some(p);
            }
        }
    }
    lock
}

/// `p` wrapped into `−π..π`.
fn princarg(p: f64) -> f64 {
    (p + PI).rem_euclid(TAU) - PI
}

/// The harmonic number of a peak at `omega` (radians per sample) for the fundamental `w0`:
/// the nearest multiple, when it is at least 1 and the peak lies within a quarter of the
/// fundamental of it.
pub(crate) fn harmonic_number(omega: f64, w0: f64) -> Option<usize> {
    let h = (omega / w0).round();
    (h >= 1.0 && (omega - h * w0).abs() < w0 / 4.0).then_some(h as usize)
}

/// The fundamental refined from the peaks (v0.3 §4.4): the mean of each harmonic peak's
/// frequency divided by its harmonic number, weighted by its magnitude. `w0` (the first guess,
/// from the period) and the result are in radians per sample; `omega` and `mag` are per bin.
/// `None` when no peak is a harmonic of `w0`.
pub(crate) fn refine_fundamental(
    peaks: &[usize],
    omega: &[f64],
    mag: &[f32],
    w0: f64,
) -> Option<f64> {
    let (sum, weight) = peaks
        .iter()
        .filter_map(|&p| harmonic_number(omega[p], w0).map(|h| (p, h)))
        .fold((0.0, 0.0), |(s, w), (p, h)| {
            let m = f64::from(mag[p]);
            (s + m * omega[p] / h as f64, w + m)
        });
    (weight > 0.0).then(|| sum / weight)
}

/// `omega` (radians per sample) rounded to the nearest frequency that completes a whole number
/// of cycles in `l` frames.
pub(crate) fn whole_cycles(omega: f64, l: usize) -> f64 {
    let per_cycle = TAU / l.max(1) as f64;
    (omega / per_cycle).round() * per_cycle
}

/// The frequency each bin advances at: its locked peak's frequency, snapped to a harmonic of
/// the fundamental `w0` when the peak is one. With a loop of `loop_len` frames, every frequency
/// completes whole cycles in it: the fundamental is rounded first (to at least one cycle), so
/// harmonics stay exact multiples of it.
fn bin_frequencies(
    raw: &[f64],
    lock: &[Option<usize>],
    w0: Option<f64>,
    loop_len: Option<usize>,
) -> Vec<f64> {
    let whole = |om: f64| loop_len.map_or(om, |l| whole_cycles(om, l));
    let w0_out = w0.map(|w| loop_len.map_or(w, |l| whole_cycles(w, l).max(TAU / l as f64)));
    lock.iter()
        .enumerate()
        .map(|(k, p)| {
            let harmonic = p
                .zip(w0)
                .zip(w0_out)
                .and_then(|((p, w), wo)| harmonic_number(raw[p], w).map(|h| h as f64 * wo));
            harmonic.unwrap_or_else(|| whole(raw[p.unwrap_or(k)]))
        })
        .collect()
}

/// The spectral analysis Smooth and Harmonic share (v0.3 §4.2, §4.4), and the lengths it
/// sets.
pub(crate) struct Spectral {
    /// The frame size (a power of two).
    pub(crate) n: usize,
    /// The hop, a quarter frame.
    pub(crate) hop: usize,
    /// The analysis frames, `[channel][frame][bin]`, the last ending at the cut.
    pub(crate) frames: Frames,
    /// Per bin: the last frame's magnitude, channels combined by energy.
    pub(crate) mag_last: Vec<f32>,
    /// The prominent peaks of `mag_last`, in ascending bin order.
    pub(crate) peaks: Vec<usize>,
    /// Per bin: the peak it is locked to, or `None` for a noise bin.
    pub(crate) lock: Vec<Option<usize>>,
    /// Per bin: the phase-vocoder frequency, in radians per sample.
    pub(crate) raw: Vec<f64>,
    /// The refined fundamental in radians per sample, for a tonal source whose peaks fit it.
    pub(crate) w0: Option<f64>,
    /// Per bin, in dB per second: the measured fall, which carries the level up to the cut.
    pub(crate) measured: Vec<f32>,
    /// Per bin, in dB per second: the target fall after the cut.
    pub(crate) target: Vec<f32>,
    /// How the level moves over the new audio.
    pub(crate) course: Course,
    /// Frames of new audio after the cut.
    pub(crate) after: usize,
    /// The Sustain body length L, a whole number of hops (0 in Decay).
    pub(crate) body: usize,
}

/// The fundamental the period of a tonal source gives, in radians per sample.
pub(crate) fn first_guess(a: &TailAnalysis) -> Option<f64> {
    a.character
        .period
        .filter(|p| p.is_finite() && *p > 0.0)
        .map(|p| TAU / f64::from(p))
}

/// Analyses the frames before the cut (v0.3 §4.2, §4.4): peaks, locking, the phase-vocoder
/// frequencies, the refined fundamental, the per-bin rates and the length of the new audio.
pub(crate) fn analyse_spectrum(
    audio: &[Vec<f32>],
    cut: usize,
    a: &TailAnalysis,
    s: &Resolved,
    rate: u32,
) -> Result<Spectral, TailError> {
    let (n, frames) = spectral_frames(audio, a.source.0, cut, rate)?;
    let hop = n / 4;
    let bins = n / 2 + 1;
    let count = frames.iter().map(Vec::len).min().unwrap_or(0);
    let (prev, last) = (count - 2, count - 1);

    let mag_last: Vec<f32> = (0..bins)
        .map(|k| energy(&frames, last, k).sqrt() as f32)
        .collect();
    let peaks = peak_bins(&mag_last);
    let lock = lock_bins(&peaks, &mag_last);
    // The phase-vocoder frequency of each bin, from the cross-spectrum summed over channels.
    let raw: Vec<f64> = (0..bins)
        .map(|k| {
            let cross: Complex32 = frames
                .iter()
                .map(|ch| ch[last][k] * ch[prev][k].conj())
                .sum();
            let centre = TAU * k as f64 / n as f64;
            centre + princarg(f64::from(cross.arg()) - centre * hop as f64) / hop as f64
        })
        .collect();
    let w0 = first_guess(a).and_then(|w| refine_fundamental(&peaks, &raw, &mag_last, w));
    let loop_len = (s.mode == TailMode::Sustain).then(|| sustain_lengths(s.body_s, rate, hop));

    let (t60, release) = match s.mode {
        TailMode::Decay => (s.decay_t60, true),
        TailMode::Sustain => (s.release_t60, s.release_t60 > 0.0),
    };
    let (measured, target) = per_bin_rates(&frames, &a.fit, Some(t60), s.darken, rate, hop);

    // The tail lasts until the slowest significant bin has faded to −90 dBFS.
    let loudest = mag_last.iter().copied().fold(0.0f32, f32::max);
    let floor = loudest * 10f32.powf(-LENGTH_RANGE_DB / 20.0);
    let slowest = mag_last
        .iter()
        .zip(&target)
        .filter(|&(&m, _)| m > floor)
        .map(|(_, &r)| r)
        .fold(f32::INFINITY, f32::min);
    let slowest = if slowest.is_finite() && slowest > 0.0 {
        slowest
    } else {
        60.0 / t60.max(f32::MIN_POSITIVE)
    };
    let fade = tail_length(a.level_at_cut_db, 60.0 / slowest, rate, hop);
    let (course, after, body) = match loop_len {
        None => (Course::Decay, fade, 0),
        Some(l) => (
            Course::Sustain {
                loop_len: l,
                release,
            },
            l + if release { fade } else { 0 },
            l,
        ),
    };
    Ok(Spectral {
        n,
        hop,
        frames,
        mag_last,
        peaks,
        lock,
        raw,
        w0,
        measured,
        target,
        course,
        after,
        body,
    })
}

/// What [`render`] synthesizes.
struct Plan {
    frozen: Frozen,
    course: Course,
    /// Frames of new audio after the cut.
    after: usize,
    /// The Sustain body length L (0 in Decay).
    body: usize,
}

/// Analyses the frames before the cut and freezes them (v0.3 §4.4).
fn plan(
    audio: &[Vec<f32>],
    cut: usize,
    a: &TailAnalysis,
    s: &Resolved,
    rate: u32,
) -> Result<Plan, TailError> {
    let sp = analyse_spectrum(audio, cut, a, s, rate)?;
    let loop_len = (sp.body > 0).then_some(sp.body);
    let mut omega = bin_frequencies(&sp.raw, &sp.lock, sp.w0, loop_len);
    let count = sp.frames.iter().map(Vec::len).min().unwrap_or(0);
    let (prev, last) = (count - 2, count - 1);
    let bins = sp.mag_last.len();

    let mut mag = Vec::with_capacity(sp.frames.len());
    let mut phasor = Vec::with_capacity(sp.frames.len());
    for ch in &sp.frames {
        mag.push(
            (0..bins)
                .map(|k| 0.5 * (ch[prev][k].norm() + ch[last][k].norm()))
                .collect(),
        );
        phasor.push(ch[last].iter().copied().map(unit).collect());
    }
    let mut noisy: Vec<bool> = sp.lock.iter().map(Option::is_none).collect();
    // DC is a steady offset, not noise: it keeps its phase and never turns. When it stands out
    // as a peak would, the bins of its main lobe that no peak claims are locked to it too.
    let lobe = if dc_stands_out(&sp.mag_last) {
        LOCK_BINS.min(bins - 1)
    } else {
        0
    };
    for k in 0..=lobe {
        if sp.lock[k].is_none() {
            noisy[k] = false;
            omega[k] = 0.0;
        }
    }
    Ok(Plan {
        frozen: Frozen {
            n: sp.n,
            hop: sp.hop,
            mag,
            phasor,
            omega,
            noisy,
            measured: sp.measured,
            target: sp.target,
        },
        course: sp.course,
        after: sp.after,
        body: sp.body,
    })
}

/// The phase of `x` as a unit phasor (1 for a silent bin).
pub(crate) fn unit(x: Complex32) -> Complex32 {
    let m = x.norm();
    if m > 0.0 {
        x / m
    } else {
        Complex32::new(1.0, 0.0)
    }
}

/// The seed of the random noise phases of frame `q`, well spread so that neither two frames
/// nor two seeds share a sequence.
fn frame_seed(seed: u64, q: usize) -> u64 {
    SplitMix64::new(SplitMix64::new(seed).next_u64() ^ q as u64).next_u64()
}

/// Overlap-adds frozen frames (v0.3 §4.4, §4.8). Frame `j` starts at `j·hop`; frame 0 is the
/// last analysis frame, so index `n` of the result is the cut. Returns `n + after` frames per
/// channel; the first three hops are not fully covered by frames, and only the last hop before
/// the cut and what follows are meant to be used.
///
/// - Tonal bins advance their phase at their own frequency. In Sustain the frame position is
///   taken modulo the loop length, so frames one loop apart are bit-identical.
/// - Noisy bins take a seeded random phase offset per frame, the same in every channel, and a
///   gain of √(n / hop) that keeps their power. In Sustain the offsets repeat every
///   `loop_len / hop` frames.
/// - The level follows `course`, per bin.
pub(crate) fn synthesize(
    f: &Frozen,
    course: Course,
    after: usize,
    rate: u32,
    seed: u64,
) -> Vec<Vec<f32>> {
    let (n, hop) = (f.n, f.hop.max(1));
    let total = n + after;
    let count = total.div_ceil(hop).max(1);
    let len = (count - 1) * hop + n;
    let fft = Fft::new(n);
    let w = hann(n);
    let bins = f.omega.len();
    let r = f64::from(rate.max(1));
    let half = (n / 2) as f64;
    // Frames with random phases add by energy, not by amplitude. Each spreads its windowed
    // energy evenly over the frame, so after the second window and the division by Σw² the
    // noise would come out at mean(w²) / Σw² = hop / n of its power. This restores it.
    let noise_gain = (n as f32 / hop as f32).sqrt();
    let (wrap, loop_end, release) = match course {
        Course::Decay => (None, 0, false),
        Course::Sustain { loop_len, release } => {
            let l = loop_len.max(hop) / hop * hop;
            (Some(l), n + l - 1, release)
        }
    };
    let mut out = vec![vec![0.0f32; len]; f.mag.len()];
    let mut norm = vec![0.0f32; len];
    let mut gain = vec![0.0f32; bins];
    let mut rot = vec![Complex32::default(); bins];
    let mut spec = vec![Complex32::default(); bins];
    for j in 0..count {
        let start = j * hop;
        // The level at this frame, per bin: the fall in dB, then the gain.
        for (k, g) in gain.iter_mut().enumerate() {
            let (meas, targ) = (f64::from(f.measured[k]), f64::from(f.target[k]));
            let db = match wrap {
                None => {
                    // The frame centre relative to the cut.
                    let c = start as f64 - half;
                    meas * (c.min(0.0) + half) + targ * c.max(0.0)
                }
                Some(_) => {
                    // Frames starting after the loop end fade from it, measured at their
                    // centres as in Decay; the frames that overlap the loop (the window is 0 at
                    // a frame's first sample) keep its level, so the loop is never changed.
                    let past = if release && start > loop_end {
                        start as f64 + half - loop_end as f64
                    } else {
                        0.0
                    };
                    meas * half + targ * past
                }
            };
            *g = 10f64.powf(-db / r / 20.0) as f32;
        }
        // The phase advance, per bin.
        let pos = wrap.map_or(start, |l| start % l);
        let q = wrap.map_or(j, |l| j % (l / hop));
        let mut rng = SplitMix64::new(frame_seed(seed, q));
        for (k, z) in rot.iter_mut().enumerate() {
            *z = if f.noisy[k] {
                Complex32::from_polar(noise_gain, rng.phase())
            } else {
                let a = f.omega[k] * pos as f64;
                Complex32::new(a.cos() as f32, a.sin() as f32)
            };
        }
        for (c, ch) in out.iter_mut().enumerate() {
            for (k, x) in spec.iter_mut().enumerate() {
                *x = f.phasor[c][k] * rot[k] * (f.mag[c][k] * gain[k]);
            }
            let frame = fft.inverse(&spec);
            for ((o, v), wi) in ch[start..start + n].iter_mut().zip(&frame).zip(&w) {
                *o += v * wi;
            }
        }
        for (o, wi) in norm[start..start + n].iter_mut().zip(&w) {
            *o += wi * wi;
        }
    }
    for ch in &mut out {
        for (v, nv) in ch.iter_mut().zip(&norm) {
            *v /= nv.max(1e-6);
        }
        ch.truncate(total);
    }
    out
}

/// The replacement for `cut − join` onwards: the original crossfades linearly into the
/// synthesized audio over the `join` frames before the cut (v0.3 §4.6), then the synthesized
/// audio after the cut follows. `synth` is in [`synthesize`]'s layout, the cut at index `n`,
/// and `join` is at most a hop.
pub(crate) fn assemble(
    audio: &[Vec<f32>],
    cut: usize,
    synth: &[Vec<f32>],
    n: usize,
    join: usize,
) -> Vec<Vec<f32>> {
    audio
        .iter()
        .zip(synth)
        .map(|(ch, sy)| {
            let mut y = Vec::with_capacity(join + sy.len().saturating_sub(n));
            for i in 0..join {
                let t = (i as f32 + 0.5) / join as f32;
                y.push(ch[cut - join + i] * (1.0 - t) + sy[n - join + i] * t);
            }
            y.extend_from_slice(&sy[n..]);
            y
        })
        .collect()
}

/// Renders the tail after `cut` by freezing the spectrum there.
///
/// `SourceTooShort` when the region before the cut cannot hold two analysis frames, which only
/// happens at very low sample rates.
pub(super) fn render(
    audio: &[Vec<f32>],
    cut: usize,
    a: &TailAnalysis,
    s: &Resolved,
    e: &EditState,
    rate: u32,
) -> Result<Generated, TailError> {
    let p = plan(audio, cut, a, s, rate)?;
    let f = &p.frozen;
    let synth = synthesize(f, p.course, p.after, rate, s.seed);
    Ok(finish(audio, cut, a, e, &synth, (f.n, f.hop), p.body, rate))
}

/// Joins synthesized audio onto the original and packages it, for Smooth and Harmonic.
/// `synth` is in [`synthesize`]'s layout for frames of `n` at a hop of `hop`, and `body` is the
/// Sustain body length (0 in Decay).
///
/// - The join is a quarter frame, or whatever room an existing loop leaves (v0.3 §4.6), with
///   `OverlapShortened` when that is less.
/// - What follows the body (the whole tail in Decay, the release in Sustain) ends in a 5 ms
///   ramp to zero.
/// - A source the analysis calls noisy gives `NoisySourceForMethod` (v0.3 §7.1).
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish(
    audio: &[Vec<f32>],
    cut: usize,
    a: &TailAnalysis,
    e: &EditState,
    synth: &[Vec<f32>],
    (n, hop): (usize, usize),
    body: usize,
    rate: u32,
) -> Generated {
    let mut warnings = Vec::new();
    if a.character.period.is_none() {
        warnings.push(TailWarning::NoisySourceForMethod);
    }
    let join = join_overlap(e, cut, hop, 1).unwrap_or(0);
    if join != hop {
        warnings.push(TailWarning::OverlapShortened);
    }
    let mut out = assemble(audio, cut, synth, n, join);
    for ch in &mut out {
        let ending = &mut ch[join + body..];
        if !ending.is_empty() {
            end_ramp(ending, rate);
        }
    }
    Generated {
        audio: out,
        overlap: join,
        body,
        loop_crossfade: 0,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::super::measure::*;
    use super::super::test_signals::*;
    use super::super::*;
    use super::{Course, plan, synthesize};
    use crate::dsp::spectrum::welch_power;

    fn decay() -> TailSettings {
        TailSettings {
            method: TailMethod::Smooth,
            ..TailSettings::default()
        }
    }

    fn sustain() -> TailSettings {
        TailSettings {
            mode: TailMode::Sustain,
            body_s: 1.0,
            ..decay()
        }
    }

    #[test]
    fn smooth_join_and_level() {
        for (name, x, cut) in all() {
            for s in [decay(), sustain()] {
                let (r, out) = run(&x, &cut_at(&x, cut), &s);
                let src = r.analysis.source;
                let mode = s.mode;
                let at_cut = join_score(&out, cut, &x, src, RATE);
                let at_start = join_score(&out, r.step.first, &x, src, RATE);
                assert!(at_cut <= 1.5, "{name} {mode:?}: join at the cut {at_cut}");
                assert!(
                    at_start <= 1.5,
                    "{name} {mode:?}: join at the overlap start {at_start}"
                );
                let fall = match mode {
                    TailMode::Decay => tail_rate(&r),
                    TailMode::Sustain => 0.0,
                };
                let step = level_step_db(&out, cut, r.analysis.level_at_cut_db, RATE, fall);
                assert!(step.abs() <= 1.5, "{name} {mode:?}: level step {step} dB");
                if mode == TailMode::Sustain {
                    let ed = &r.edit;
                    assert_eq!(ed.loop_mode, LoopMode::Forward, "{name}");
                    assert_eq!((ed.loop_start, ed.crossfade), (cut, 0), "{name}");
                    let wrap = wrap_score(&out, (ed.loop_start, ed.loop_end), &x, src, RATE);
                    assert!(wrap <= 1.5, "{name}: wrap {wrap}");
                    // The release starts with what a wrap plays.
                    let (a, b) = (ed.loop_start, ed.loop_end);
                    for ch in &out {
                        assert!((ch[b + 1] - ch[a]).abs() < 1e-5, "{name}");
                    }
                    assert!(out.iter().all(|ch| ch[ch.len() - 1] == 0.0), "{name}");
                } else {
                    assert!(out.iter().all(|ch| ch[ch.len() - 1] == 0.0), "{name}");
                }
            }
        }
    }

    #[test]
    fn smooth_sustain_is_steady_and_keeps_its_shape() {
        for (name, x, cut) in all() {
            let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
            let (a, b) = (r.edit.loop_start, r.edit.loop_end);
            let spread = body_spread_db(&out, a, b, RATE);
            assert!(spread <= 1.0, "{name}: body level spread {spread} dB");
            let dev = level_deviation_db(&out, a, b, r.analysis.level_at_cut_db, RATE);
            if name == "steady tone" {
                // The 20 ms level at the cut holds under two periods of this 97 Hz pulse wave
                // and reads 1.3 dB below its true level, so the uncut tone's own 50 ms levels
                // stray up to 1.4 dB from it. The general bound holds against it, and 1 dB
                // holds against the level of the whole source region.
                assert!(dev <= 1.5, "{name}: body level strays {dev} dB");
                let (s0, s1) = r.analysis.source;
                let true_db = crate::dsp::envelope::level_db(&x, s0, s1);
                let dev = level_deviation_db(&out, a, b, true_db, RATE);
                assert!(
                    dev <= 1.0,
                    "{name}: body level strays {dev} dB from the source's"
                );
            } else {
                assert!(dev <= 1.0, "{name}: body level strays {dev} dB");
            }
        }

        let (x, cut) = steady_tone();
        let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
        let (a, b) = (r.edit.loop_start, r.edit.loop_end);
        let t = r.analysis.character.period.unwrap().round() as usize;
        let y = &out[0];
        let corr = |p: usize| {
            let (u, v) = (&y[p..p + t], &y[p + t..p + 2 * t]);
            let uv: f32 = u.iter().zip(v).map(|(a, b)| a * b).sum();
            let uu: f32 = u.iter().map(|a| a * a).sum();
            let vv: f32 = v.iter().map(|a| a * a).sum();
            uv / (uu * vv).sqrt()
        };
        let worst = (a..=b + 1 - 2 * t)
            .step_by(t)
            .map(corr)
            .fold(f32::MAX, f32::min);
        assert!(worst >= 0.98, "consecutive periods correlate at {worst}");

        let peak = |s: &[f32]| s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let (s0, s1) = r.analysis.source;
        let ratio_db = 20.0 * (peak(&y[a..=b]) / peak(&x[0][s0..=s1])).log10();
        assert!(
            ratio_db.abs() <= 3.0,
            "peak {ratio_db} dB from the source's"
        );
    }

    #[test]
    fn smooth_keeps_a_dc_offset_steady() {
        // A 98 Hz tone (a period of exactly 225 frames) on a DC offset of 0.2.
        let dc = 0.2f32;
        let x = vec![
            (0..at(1.5))
                .map(|i| dc + 0.3 * (std::f32::consts::TAU * i as f32 / 225.0).sin())
                .collect::<Vec<f32>>(),
        ];
        let cut = at(1.2);
        let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
        let (a, b) = (r.edit.loop_start, r.edit.loop_end);
        // Means over four periods, where the tone cancels: the DC alone, without a random
        // sub-20 Hz wander around it.
        let block = 4 * 225;
        let means: Vec<f32> = out[0][a..=b]
            .chunks_exact(block)
            .map(|c| c.iter().sum::<f32>() / block as f32)
            .collect();
        let mean = means.iter().sum::<f32>() / means.len() as f32;
        assert!((mean - dc).abs() < 0.01, "mean {mean}, source DC {dc}");
        let worst = means.iter().map(|m| (m - dc).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.02, "the DC wanders by {worst}: {means:?}");
    }

    #[test]
    fn smooth_release_starts_at_the_loop_end() {
        // A steady tone with a 0.5 s release: 120 dB/s from the loop end (v0.3 §4.8). Each
        // 10 ms window after the loop end is compared with the same window of the loop, which
        // the release continues, so the tone's own ripple cancels.
        let (x, cut) = steady_tone();
        let s = TailSettings {
            release_t60: Some(0.5),
            ..sustain()
        };
        let (r, out) = run(&x, &cut_at(&x, cut), &s);
        let (a, b) = (r.edit.loop_start, r.edit.loop_end);
        let win = at(0.010);
        let level = |first: usize| crate::dsp::envelope::level_db(&out, first, first + win - 1);
        let fall = |d: usize| level(b + 1 + d) - level(a + d);
        let expected = |d: usize| -120.0 * (d + win / 2) as f32 / RATE as f32;
        // The frames that overlap the loop keep its level, so the first few milliseconds
        // barely move; already below the body, never above it.
        for d in [0, at(0.010)] {
            assert!(
                fall(d) <= 0.0,
                "{} ms: {:+.2} dB",
                d * 1000 / at(1.0),
                fall(d)
            );
        }
        // Half a frame (~45 ms) in, the release is well under way (a fade timed from each
        // frame's start was still at the body level here).
        let d = at(0.040);
        assert!(fall(d) < -2.5, "40 ms: {:+.2} dB", fall(d));
        // From one frame (~93 ms) on, it follows 120 dB/s from the loop end (a fade timed
        // from each frame's start lagged about 6 dB behind).
        for d in (at(0.095)..at(0.3)).step_by(win) {
            let (got, want) = (fall(d), expected(d));
            assert!(
                (got - want).abs() <= 1.0,
                "{} ms: {got:+.2} dB, expected {want:+.2}",
                d * 1000 / at(1.0)
            );
        }
    }

    #[test]
    fn smooth_sustain_loop_is_exactly_periodic() {
        for (name, x, cut) in all() {
            let e = cut_at(&x, cut);
            let s = sustain();
            let a = analyse(&x, &e, None, RATE).unwrap();
            let resolved = resolve(&s, &a, RATE);
            let p = plan(&x, cut, &a, &resolved, RATE).unwrap();
            let (n, hop) = (p.frozen.n, p.frozen.hop);
            let l = p.body;
            assert_eq!(l % hop, 0, "{name}: L is whole hops");
            let course = Course::Sustain {
                loop_len: l,
                release: false,
            };
            let y = synthesize(&p.frozen, course, 2 * l, RATE, s.seed);
            for ch in &y {
                let worst = (0..l)
                    .map(|i| (ch[n + i] - ch[n + l + i]).abs())
                    .fold(0.0f32, f32::max);
                assert!(worst <= 1e-5, "{name}: halves differ by {worst}");
            }
            // The render's loop is that body.
            let r = render_tail(&x, &e, None, RATE, &s).unwrap();
            assert_eq!(r.edit.loop_len(), l, "{name}");
        }
    }

    #[test]
    fn smooth_highs_fade_faster_with_darken() {
        let (x, cut) = noise();
        let e = cut_at(&x, cut);
        let dark = TailSettings {
            darken: 1.0,
            ..decay()
        };
        let (_, d) = run(&x, &e, &dark);
        let (_, p) = run(&x, &e, &decay());
        let n = 1024;
        let highs = |y: &[Vec<f32>]| {
            let from = cut + at(0.5);
            let pw = welch_power(&y[0], from, from + at(0.1), n);
            let k0 = 2000 * n / RATE as usize + 1;
            pw[k0..].iter().map(|&v| f64::from(v)).sum::<f64>()
        };
        let drop = 10.0 * (highs(&p) / highs(&d)).log10();
        assert!(drop >= 6.0, "highs only {drop} dB lower with Darken");
    }

    #[test]
    fn smooth_follows_the_real_ending() {
        let (x, cut) = decaying_tone();
        let (_, out) = run(&x, &cut_at(&x, cut), &decay());
        let err = envelope_error_db(&out, cut, &x, cut, RATE);
        assert!(err <= 3.0, "envelope error {err} dB");
    }

    #[test]
    fn tail_is_deterministic() {
        for (name, x, cut, s) in [
            ("noise", noise(), decay()),
            ("stereo tone", stereo_tone(), sustain()),
        ]
        .map(|(name, (x, cut), s)| (name, x, cut, s))
        {
            let e = cut_at(&x, cut);
            let a = render_tail(&x, &e, None, RATE, &s).unwrap();
            let b = render_tail(&x, &e, None, RATE, &s).unwrap();
            assert_eq!(a, b, "{name}");
            let c = render_tail(&x, &e, None, RATE, &TailSettings { seed: 2, ..s }).unwrap();
            assert_ne!(a.step, c.step, "{name}: a new seed gives new audio");
        }
    }

    #[test]
    fn analysis_suggests_smooth_for_tonal() {
        for (name, x, cut) in all() {
            let a = analyse(&x, &cut_at(&x, cut), None, RATE).unwrap();
            let want = if a.character.period.is_some() {
                TailMethod::Smooth
            } else {
                TailMethod::Granular
            };
            assert_eq!(a.suggested_method, want, "{name}");
        }
        let (x, cut) = steady_tone();
        let a = analyse(&x, &cut_at(&x, cut), None, RATE).unwrap();
        assert_eq!(a.suggested_method, TailMethod::Smooth);
    }

    #[test]
    fn smooth_on_noise_warns() {
        let (x, cut) = noise();
        for s in [decay(), sustain()] {
            let (r, _) = run(&x, &cut_at(&x, cut), &s);
            assert!(
                r.warnings.contains(&TailWarning::NoisySourceForMethod),
                "{:?}",
                r.warnings
            );
        }
        let (x, cut) = steady_tone();
        let (r, _) = run(&x, &cut_at(&x, cut), &decay());
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn smooth_keeps_stereo_correlation() {
        let (x, cut) = stereo_tone();
        for s in [decay(), sustain()] {
            let (r, out) = run(&x, &cut_at(&x, cut), &s);
            let (s0, s1) = r.analysis.source;
            let want = channel_correlation(&x, s0, s1);
            let got = channel_correlation(&out, cut, out[0].len() - 1);
            assert!((got - want).abs() <= 0.1, "{:?}: {got} vs {want}", s.mode);
        }
    }

    #[test]
    fn smooth_never_touches_an_existing_loop() {
        for (name, x, cut) in all() {
            // The loop end right at the cut: no room at all; then 100 frames of room.
            for room in [0, 100] {
                let mut e = cut_at(&x, cut);
                e.loop_mode = LoopMode::Forward;
                e.loop_start = cut - at(0.3);
                e.loop_end = cut - 1 - room;
                e.crossfade = 64;
                let (r, out) = run(&x, &e, &decay());
                assert!(r.step.first > e.loop_end, "{name}, room {room}");
                assert_eq!(cut - r.step.first, room, "{name}: crossfades over the room");
                for (o, i) in out.iter().zip(&x) {
                    assert_eq!(o[..=e.loop_end], i[..=e.loop_end], "{name}, room {room}");
                }
                assert!(
                    r.warnings.contains(&TailWarning::OverlapShortened),
                    "{name}, room {room}: {:?}",
                    r.warnings
                );
                let ed = &r.edit;
                assert_eq!(
                    (ed.loop_mode, ed.loop_start, ed.loop_end, ed.crossfade),
                    (e.loop_mode, e.loop_start, e.loop_end, e.crossfade)
                );
            }
        }
    }

    #[test]
    fn tiny_sources_at_very_low_rates_are_refused_or_render() {
        // At 50 Hz, 30 ms is a frame and a half: no 4-point frame fits, so the source is too
        // short. A few more frames either render finite audio or are refused, never panic.
        for frames in [2, 3, 12, 40] {
            let x = vec![
                (0..frames)
                    .map(|i| ((i * 7) % 5) as f32 * 0.2 - 0.4)
                    .collect::<Vec<f32>>(),
            ];
            let e = EditState::default_for(frames);
            for s in [decay(), sustain()] {
                match render_tail(&x, &e, None, 50, &s) {
                    Ok(r) => {
                        let Op::Splice(sp) = &r.step.op else {
                            panic!("not a splice")
                        };
                        assert!(sp.audio.iter().flatten().all(|v| v.is_finite()));
                    }
                    Err(err) => {
                        assert_eq!(err, TailError::SourceTooShort, "{frames} frames");
                        assert!(frames <= 3, "{frames} frames refused");
                    }
                }
            }
        }
    }

    /// Times a 30 s stereo tail from a 1 s 48 kHz source: `cargo test --release -p
    /// limewave-core smooth_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn smooth_timing() {
        let rate = 48_000;
        let mut rng = crate::dsp::rng::SplitMix64::new(4);
        let x: Vec<Vec<f32>> = (0..2)
            .map(|c| {
                (0..rate as usize)
                    .map(|i| {
                        let t = i as f32 / rate as f32;
                        0.3 * (std::f32::consts::TAU * 110.0 * t + c as f32).sin()
                            + 0.01 * (rng.next_f32() - 0.5)
                    })
                    .collect()
            })
            .collect();
        let e = EditState::default_for(x[0].len());
        for s in [
            TailSettings {
                decay_t60: Some(100.0),
                ..decay()
            },
            TailSettings {
                body_s: 30.0,
                release_t60: Some(0.0),
                ..sustain()
            },
        ] {
            let t = std::time::Instant::now();
            let r = render_tail(&x, &e, None, rate, &s).unwrap();
            let Op::Splice(sp) = &r.step.op else {
                panic!("not a splice")
            };
            println!(
                "{:?}: {:.2} s of tail in {:?}",
                s.mode,
                sp.audio[0].len() as f32 / rate as f32,
                t.elapsed()
            );
        }
    }

    /// Prints the §8.1 numbers for the report: `cargo test -p limewave-core smooth_numbers
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn smooth_numbers() {
        for (name, x, cut) in all() {
            let (r, out) = run(&x, &cut_at(&x, cut), &decay());
            let src = r.analysis.source;
            println!(
                "{name} decay: join {:.2}, overlap start {:.2}, level step {:+.2} dB, tail {:.2} s, corr {:.3} vs {:.3}, warnings {:?}",
                join_score(&out, cut, &x, src, RATE),
                join_score(&out, r.step.first, &x, src, RATE),
                level_step_db(&out, cut, r.analysis.level_at_cut_db, RATE, tail_rate(&r)),
                (out[0].len() - cut) as f32 / RATE as f32,
                channel_correlation(&out, cut, out[0].len() - 1),
                channel_correlation(&x, src.0, src.1),
                r.warnings,
            );
            if name == "decaying tone" {
                println!(
                    "  envelope error {:.2} dB",
                    envelope_error_db(&out, cut, &x, cut, RATE)
                );
            }
            let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
            let ed = &r.edit;
            println!(
                "{name} sustain: join {:.2}, level step {:+.2} dB, wrap {:.2}, body dev {:.2} dB, spread {:.2} dB, body {} frames",
                join_score(&out, cut, &x, src, RATE),
                level_step_db(&out, cut, r.analysis.level_at_cut_db, RATE, 0.0),
                wrap_score(&out, (ed.loop_start, ed.loop_end), &x, src, RATE),
                level_deviation_db(
                    &out,
                    ed.loop_start,
                    ed.loop_end,
                    r.analysis.level_at_cut_db,
                    RATE
                ),
                body_spread_db(&out, ed.loop_start, ed.loop_end, RATE),
                ed.loop_len(),
            );
        }
    }
}
