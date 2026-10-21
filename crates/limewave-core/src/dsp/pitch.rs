//! Pitch and tonal character from normalized autocorrelation.

use realfft::RealFftPlanner;

/// The highest pitch searched, in Hz: the shortest period is `rate / 2000` samples.
const HIGHEST_HZ: f64 = 2000.0;

/// The lowest pitch searched, in Hz: the longest period is `rate / 40` samples.
const LOWEST_HZ: f64 = 40.0;

/// A peak at least this fraction of the best one, at a shorter lag, is preferred over it, so
/// a multiple of the period is not picked by accident.
const NEAR_BEST: f64 = 0.9;

/// Whether a sound repeats, and how strongly, from [`character`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Character {
    /// The period in samples (fractional), present only when `strength` reaches the
    /// threshold: the sound is tonal.
    pub period: Option<f32>,
    /// The normalized autocorrelation at the chosen lag, 0 to 1. 1 is a perfect repeat.
    pub strength: f32,
    /// The chosen lag in whole samples, kept even when the sound is noisy (it sizes windows
    /// for low sounds). `None` only when there is no audio or no dip below zero to search
    /// after.
    pub strongest_lag: Option<usize>,
}

/// The tonal character of `first..=last` (inclusive, clamped to the shortest channel), from
/// the normalized autocorrelation summed over channels (v0.3 §4.2).
///
/// - Channels are correlated separately and summed, so out-of-phase channels do not cancel.
///   The value at lag `L` is `Σ x[t]·x[t+L] / √(Σ x[t]² · Σ x[t+L]²)` over the overlapping
///   part, summed over channels.
/// - Lags from `rate / 2000` to `rate / 40` (2 kHz down to 40 Hz) are searched, and at most
///   half the region.
/// - The search starts after the first lag where the autocorrelation dips below zero, so the
///   main lobe of a smooth, low sound does not read as a very short period.
/// - Among local peaks within 90 % of the best, the shortest lag wins.
/// - Parabolic interpolation around the chosen lag gives the fractional `period`, which is
///   reported only when the strength is at least `threshold` (0.6 for tails, 0.45 for spectrum
///   loops).
///
/// Silence, too little audio or a zero `rate` give [`Character::default`].
pub fn character(
    audio: &[Vec<f32>],
    first: usize,
    last: usize,
    rate: u32,
    threshold: f32,
) -> Character {
    let none = Character::default();
    let Some(len) = audio.iter().map(Vec::len).min() else {
        return none;
    };
    let last = last.min(len.saturating_sub(1));
    if len == 0 || first > last || rate == 0 {
        return none;
    }
    let n = last + 1 - first;
    let rate = f64::from(rate);
    let lo = ((rate / HIGHEST_HZ) as usize).max(2);
    let hi = ((rate / LOWEST_HZ) as usize).min(n / 2);
    if hi < lo {
        return none;
    }
    let r = normalized_autocorrelation(audio, first, n, hi + 1);
    let Some(dip) = (1..=hi).find(|&l| r[l] < 0.0) else {
        return none;
    };
    let start = dip.max(lo);
    if start > hi {
        return none;
    }
    // The first of equal maxima, so the shortest lag wins ties too.
    let best = (start..=hi).fold(start, |b, l| if r[l] > r[b] { l } else { b });
    let k = (start + 1..best)
        .find(|&j| r[j] >= NEAR_BEST * r[best] && r[j] >= r[j - 1] && r[j] >= r[j + 1])
        .unwrap_or(best);
    let strength = r[k].max(0.0) as f32;
    Character {
        period: (strength >= threshold).then(|| (k as f64 + parabolic_offset(&r, k)) as f32),
        strength,
        strongest_lag: Some(k),
    }
}

/// Where the true peak lies relative to `k`, from a parabola through `r[k − 1..=k + 1]`,
/// within ±0.5. 0 when `k` has no neighbour on both sides or the parabola has no maximum.
fn parabolic_offset(r: &[f64], k: usize) -> f64 {
    if k == 0 || k + 1 >= r.len() {
        return 0.0;
    }
    let (a, b, c) = (r[k - 1], r[k], r[k + 1]);
    let curve = a - 2.0 * b + c;
    if curve < 0.0 {
        (0.5 * (a - c) / curve).clamp(-0.5, 0.5)
    } else {
        0.0
    }
}

/// The normalized autocorrelation of `n` frames from `first`, summed over channels, for lags
/// `0..=max_lag` (`max_lag < n`), each clamped to −1..1 and 0 where either part is silent.
///
/// The raw correlation comes from an FFT of the zero-padded signal, in `f64` so its rounding
/// stays far below the energies it is divided by; the energies come from prefix sums.
fn normalized_autocorrelation(
    audio: &[Vec<f32>],
    first: usize,
    n: usize,
    max_lag: usize,
) -> Vec<f64> {
    // Padding to n + max_lag keeps the circular correlation from wrapping into the lags used.
    let m = (n + max_lag + 1).next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(m);
    let inv = planner.plan_fft_inverse(m);
    let mut raw = vec![0.0f64; max_lag + 1];
    // prefix[i]: energy of the first i frames, summed over channels.
    let mut prefix = vec![0.0f64; n + 1];
    for ch in audio {
        let x = &ch[first..first + n];
        let mut buf = fwd.make_input_vec();
        for (b, &v) in buf.iter_mut().zip(x) {
            *b = f64::from(v);
        }
        for (i, b) in buf[..n].iter().enumerate() {
            prefix[i + 1] += b * b;
        }
        let mut spec = fwd.make_output_vec();
        let _ = fwd.process(&mut buf, &mut spec);
        for c in &mut spec {
            *c = (c.norm_sqr() / m as f64).into();
        }
        let mut out = inv.make_output_vec();
        let _ = inv.process(&mut spec, &mut out);
        for (acc, v) in raw.iter_mut().zip(&out) {
            *acc += v;
        }
    }
    for i in 0..n {
        prefix[i + 1] += prefix[i];
    }
    raw.iter()
        .enumerate()
        .map(|(lag, &num)| {
            // Σ x[t]² over t < n − lag, and over lag ≤ t < n.
            let den = (prefix[n - lag] * (prefix[n] - prefix[lag])).sqrt();
            if den > 0.0 {
                (num / den).clamp(-1.0, 1.0)
            } else {
                0.0
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rng::SplitMix64;
    use std::f32::consts::TAU;

    /// `secs` of a tone at `f0` made of `harmonics` equal-amplitude harmonics.
    fn tone(rate: u32, f0: f32, harmonics: usize, secs: f32) -> Vec<f32> {
        let n = (secs * rate as f32) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                (1..=harmonics)
                    .map(|h| (TAU * f0 * h as f32 * t).cos())
                    .sum::<f32>()
                    / harmonics as f32
            })
            .collect()
    }

    #[test]
    fn character_finds_the_period_and_skips_the_main_lobe() {
        // Pulse-like: 20 equal harmonics of 97 Hz at 11025 Hz.
        let x = tone(11_025, 97.0, 20, 0.5);
        let c = character(std::slice::from_ref(&x), 0, x.len() - 1, 11_025, 0.6);
        let p = c.period.expect("tonal");
        let want = 11_025.0 / 97.0;
        assert!((p - want).abs() < 0.005 * want, "period {p}, want {want}");
        assert!(c.strength >= 0.6);

        // A smooth low tone must not read as a very short period.
        let x = tone(44_100, 55.0, 1, 0.5);
        let c = character(std::slice::from_ref(&x), 0, x.len() - 1, 44_100, 0.6);
        let p = c.period.expect("tonal");
        assert!(p > 44_100.0 / 1000.0, "period {p}");
        assert!((p - 44_100.0 / 55.0).abs() < 2.0, "period {p}");

        // White noise is not tonal, but still has a strongest repeat.
        let mut r = SplitMix64::new(9);
        let x: Vec<f32> = (0..22_050).map(|_| r.next_f32() * 2.0 - 1.0).collect();
        let c = character(std::slice::from_ref(&x), 0, x.len() - 1, 44_100, 0.6);
        assert_eq!(c.period, None);
        assert!(c.strength < 0.6);
        assert!(c.strongest_lag.is_some());
    }

    #[test]
    fn out_of_phase_stereo_is_measured_by_energy() {
        let l = tone(44_100, 220.0, 5, 0.3);
        let r: Vec<f32> = l.iter().map(|v| -v).collect();
        let c = character(&[l.clone(), r], 0, l.len() - 1, 44_100, 0.6);
        let p = c.period.expect("tonal");
        assert!((p - 44_100.0 / 220.0).abs() < 1.0, "period {p}");
    }

    #[test]
    fn character_handles_degenerate_input() {
        let none = Character {
            period: None,
            strength: 0.0,
            strongest_lag: None,
        };
        assert_eq!(character(&[], 0, 100, 44_100, 0.6), none);
        assert_eq!(character(&[vec![]], 0, 100, 44_100, 0.6), none);
        assert_eq!(character(&[vec![0.5]], 0, 0, 44_100, 0.6), none);
        assert_eq!(character(&[vec![0.0; 5000]], 0, 4999, 44_100, 0.6), none);
        assert_eq!(character(&[vec![0.0; 5000]], 0, 4999, 0, 0.6), none);
        assert_eq!(character(&[vec![0.0; 5000]], 3000, 100, 44_100, 0.6), none);
        let tiny: Vec<f32> = tone(44_100, 220.0, 3, 0.1)
            .iter()
            .map(|v| v * 1e-30)
            .collect();
        let c = character(std::slice::from_ref(&tiny), 0, tiny.len() - 1, 44_100, 0.6);
        assert!(c.strength.is_finite() && c.period.is_none_or(f32::is_finite));
    }
}
