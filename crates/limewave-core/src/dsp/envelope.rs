//! Level envelopes and decay fitting.
//!
//! Channels are combined by energy, never by summing samples, so out-of-phase channels do
//! not cancel: a level is the mean of `x²` over every sample of every channel. Ranges are
//! inclusive (`first..=last`) and clamped to the shortest channel.

/// The level reported for silence, in dB, so callers never see −∞.
pub const SILENCE_DB: f32 = -200.0;

/// Frames quieter than this are left out of the decay fit: they are noise floor or silence.
const FIT_FLOOR_DB: f32 = -80.0;

/// Length of the level frames used by [`decay_fit`], in seconds.
const FIT_FRAME_S: f64 = 0.010;

/// A whole-region slope shallower than this (dB per second) counts as steady.
const STEADY_SLOPE_DB_S: f64 = -3.0;

/// The decay rate is fitted over this most recent fraction of the region...
const RECENT_FRACTION: f64 = 0.4;

/// ...but over at least this many seconds.
const RECENT_MIN_S: f64 = 0.050;

/// How a region's level is changing, from [`decay_fit`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecayFit {
    /// True when the whole region falls more slowly than 3 dB/s: a sustained sound.
    pub steady: bool,
    /// The recent rate of fall in dB per second, never negative (a rising level reads 0).
    pub rate_db_s: f32,
    /// Seconds to fall 60 dB at `rate_db_s`; `None` when steady.
    pub t60: Option<f32>,
}

/// The inclusive range clamped to the shortest channel, or `None` when nothing is left.
fn clamp_range(audio: &[Vec<f32>], first: usize, last: usize) -> Option<(usize, usize)> {
    let len = audio.iter().map(Vec::len).min()?;
    let last = last.min(len.checked_sub(1)?);
    (first <= last).then_some((first, last))
}

/// Mean energy (`x²`) over all channels in `start..start + len`, in dB.
fn mean_energy_db(audio: &[Vec<f32>], start: usize, len: usize) -> f32 {
    let sum: f64 = audio
        .iter()
        .flat_map(|ch| &ch[start..start + len])
        .map(|&v| f64::from(v) * f64::from(v))
        .sum();
    let mean = sum / (len * audio.len()) as f64;
    // 1e-20 is SILENCE_DB.
    (10.0 * mean.max(1e-20).log10()) as f32
}

/// RMS level in dB of consecutive `hop`-sample frames of `first..=last`, starting at `first`
/// (a partial frame at the end is left out). Each frame's level is the mean energy over its
/// samples and all channels; silence reads [`SILENCE_DB`].
///
/// Empty when `hop` is 0 or the range holds no whole frame.
pub fn rms_db_frames(audio: &[Vec<f32>], first: usize, last: usize, hop: usize) -> Vec<f32> {
    let Some((first, last)) = clamp_range(audio, first, last).filter(|_| hop > 0) else {
        return Vec::new();
    };
    let frames = (last + 1 - first) / hop;
    (0..frames)
        .map(|k| mean_energy_db(audio, first + k * hop, hop))
        .collect()
}

/// RMS level in dB of `first..=last`: the mean energy over its samples and all channels.
/// [`SILENCE_DB`] for silence or an empty range.
pub fn level_db(audio: &[Vec<f32>], first: usize, last: usize) -> f32 {
    match clamp_range(audio, first, last) {
        Some((first, last)) => mean_energy_db(audio, first, last + 1 - first),
        None => SILENCE_DB,
    }
}

/// Least-squares slope in dB per second of a level envelope whose frames are `secs_per_frame`
/// apart, using only frames above [`FIT_FLOOR_DB`]. 0 with fewer than three such frames.
fn slope_db_s(db: &[f32], secs_per_frame: f64) -> f64 {
    let pts: Vec<(f64, f64)> = db
        .iter()
        .enumerate()
        .filter(|&(_, &d)| d > FIT_FLOOR_DB)
        .map(|(i, &d)| (i as f64 * secs_per_frame, f64::from(d)))
        .collect();
    if pts.len() < 3 {
        return 0.0;
    }
    let n = pts.len() as f64;
    let mt = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let md = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let cov: f64 = pts.iter().map(|p| (p.0 - mt) * (p.1 - md)).sum();
    let var: f64 = pts.iter().map(|p| (p.0 - mt) * (p.0 - mt)).sum();
    if var > 0.0 { cov / var } else { 0.0 }
}

/// Whether `first..=last` is steady or decaying, and how fast it is falling at the end
/// (v0.3 §4.2).
///
/// - The level envelope is RMS in 10 ms frames, aligned so the last frame ends at `last`;
///   frames below −80 dB are ignored.
/// - **Steady**: a straight-line fit over the whole region is shallower than −3 dB/s.
/// - **Rate**: the same fit over only the most recent 40 % of the region (at least 50 ms),
///   and never shallower than the whole-region slope, since decays usually speed up.
///
/// Too little audio, silence or a zero `rate` reads as steady with a rate of 0.
pub fn decay_fit(audio: &[Vec<f32>], first: usize, last: usize, rate: u32) -> DecayFit {
    let steady = DecayFit {
        steady: true,
        rate_db_s: 0.0,
        t60: None,
    };
    let Some((first, last)) = clamp_range(audio, first, last).filter(|_| rate > 0) else {
        return steady;
    };
    let hop = ((f64::from(rate) * FIT_FRAME_S) as usize).max(1);
    let frames = (last + 1 - first) / hop;
    if frames == 0 {
        return steady;
    }
    let db = rms_db_frames(audio, last + 1 - frames * hop, last, hop);
    let secs = hop as f64 / f64::from(rate);
    let whole = slope_db_s(&db, secs);
    let keep = ((RECENT_MIN_S / secs).ceil() as usize)
        .max((RECENT_FRACTION * frames as f64).ceil() as usize)
        .min(frames);
    let recent = slope_db_s(&db[frames - keep..], secs).min(whole);
    let rate_db_s = (-recent).max(0.0) as f32;
    let is_steady = whole > STEADY_SLOPE_DB_S;
    DecayFit {
        steady: is_steady,
        rate_db_s,
        // Not steady means rate_db_s >= 3, so this is finite.
        t60: (!is_steady).then(|| 60.0 / rate_db_s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const RATE: u32 = 44_100;

    /// A 220 Hz tone with a few harmonics, whose level in dB follows `gain_db(t)`.
    fn tone(secs: f32, gain_db: impl Fn(f32) -> f32) -> Vec<f32> {
        let n = (secs * RATE as f32) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let g = 10f32.powf(gain_db(t) / 20.0);
                let x: f32 = (1..=4)
                    .map(|h| (TAU * 220.0 * h as f32 * t).sin() / h as f32)
                    .sum();
                0.5 * g * x
            })
            .collect()
    }

    #[test]
    fn decay_fit_measures_a_known_decay() {
        let x = tone(1.0, |t| -60.0 * t / 1.2);
        let fit = decay_fit(std::slice::from_ref(&x), 0, x.len() - 1, RATE);
        assert!(!fit.steady);
        let t60 = fit.t60.expect("decaying");
        assert!((t60 - 1.2).abs() < 0.15 * 1.2, "t60 {t60}");
        assert!((fit.rate_db_s - 50.0).abs() < 0.15 * 50.0);

        let x = tone(1.0, |_| 0.0);
        let fit = decay_fit(std::slice::from_ref(&x), 0, x.len() - 1, RATE);
        assert!(fit.steady);
        assert_eq!(fit.t60, None);
        assert!(fit.rate_db_s >= 0.0);
    }

    #[test]
    fn decay_fit_uses_the_recent_rate() {
        // 15 dB/s for 0.6 s, then 120 dB/s for 0.4 s.
        let x = tone(1.0, |t| {
            if t < 0.6 {
                -15.0 * t
            } else {
                -9.0 - 120.0 * (t - 0.6)
            }
        });
        let fit = decay_fit(std::slice::from_ref(&x), 0, x.len() - 1, RATE);
        assert!(!fit.steady);
        assert!((fit.rate_db_s - 120.0).abs() < 0.15 * 120.0, "{fit:?}");
        let t60 = fit.t60.unwrap();
        assert!((t60 - 0.5).abs() < 0.15 * 0.5, "t60 {t60}");
    }

    #[test]
    fn out_of_phase_stereo_is_measured_by_energy() {
        let l = tone(0.5, |_| -6.0);
        let r: Vec<f32> = l.iter().map(|v| -v).collect();
        let last = l.len() - 1;
        let mono = level_db(std::slice::from_ref(&l), 0, last);
        let stereo = level_db(&[l.clone(), r.clone()], 0, last);
        assert!(mono > -20.0);
        assert!((mono - stereo).abs() < 1e-3, "{mono} vs {stereo}");
        let frames = rms_db_frames(&[l, r], 0, last, 441);
        assert!(frames.iter().all(|&d| (d - mono).abs() < 1.0));
    }

    #[test]
    fn envelope_handles_degenerate_input() {
        let silent = vec![vec![0.0f32; 1000]];
        let tiny = vec![vec![1e-30f32; 1000]];
        let cases: [Vec<Vec<f32>>; 5] =
            [silent.clone(), tiny, vec![], vec![vec![]], vec![vec![0.5]]];
        for audio in &cases {
            let l = level_db(audio, 0, 999);
            assert!(l.is_finite());
            assert!(
                rms_db_frames(audio, 0, 999, 100)
                    .iter()
                    .all(|d| d.is_finite())
            );
            let fit = decay_fit(audio, 0, 999, RATE);
            assert!(fit.rate_db_s.is_finite() && fit.rate_db_s >= 0.0);
            assert!(fit.t60.is_none_or(f32::is_finite));
        }
        assert!(rms_db_frames(&silent, 0, 999, 0).is_empty());
        assert!(rms_db_frames(&silent, 500, 100, 10).is_empty());
        assert!(decay_fit(&silent, 0, 999, 0).rate_db_s == 0.0);
    }
}
