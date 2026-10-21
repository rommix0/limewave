//! Real FFTs, Hann windows, framing and averaged power spectra.
//!
//! Frames are always Hann-windowed and advance by a quarter frame (`n / 4`), the hop at which
//! a periodic Hann window overlap-adds to a constant. Spectra are unnormalized: a unit-amplitude
//! sine centred on a bin gives `|X| = n / 4` there.

use std::f64::consts::TAU;
use std::sync::Arc;

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Complex sample types, re-exported so callers need no extra dependency.
pub use realfft::num_complex::{Complex32, Complex64};

/// A periodic Hann window of `n` points: `0.5 − 0.5·cos(2πi/n)`, starting at 0 and peaking at
/// 1 at `n / 2`. Empty for `n = 0`.
pub fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (0.5 - 0.5 * (TAU * i as f64 / n as f64).cos()) as f32)
        .collect()
}

/// Forward and inverse plans of one size.
type Plans = (Arc<dyn RealToComplex<f32>>, Arc<dyn ComplexToReal<f32>>);

/// A planned real FFT of one size, reusable for any number of transforms.
pub struct Fft {
    n: usize,
    plans: Option<Plans>,
}

impl Fft {
    /// Plans forward and inverse transforms of `n` points. Any `n` works, including odd sizes
    /// and 0 (which transforms nothing), though powers of two are fastest.
    pub fn new(n: usize) -> Fft {
        let plans = (n > 0).then(|| {
            let mut planner = RealFftPlanner::<f32>::new();
            (planner.plan_fft_forward(n), planner.plan_fft_inverse(n))
        });
        Fft { n, plans }
    }

    /// The transform size.
    pub fn len(&self) -> usize {
        self.n
    }

    /// True for a zero-point transform.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The `n / 2 + 1` bins from DC to the Nyquist frequency of `x`, unnormalized. `x` is
    /// zero-padded or truncated to `n` points.
    pub fn forward(&self, x: &[f32]) -> Vec<Complex32> {
        let Some((fwd, _)) = &self.plans else {
            return vec![Complex32::default()];
        };
        let mut input = fwd.make_input_vec();
        let k = x.len().min(self.n);
        input[..k].copy_from_slice(&x[..k]);
        let mut out = fwd.make_output_vec();
        // Only fails on buffer lengths, which the plan made.
        let _ = fwd.process(&mut input, &mut out);
        out
    }

    /// The `n` samples whose [`forward`](Self::forward) transform is `spec`, scaled by `1 / n`
    /// so the round trip gives back the input. `spec` is zero-padded or truncated to
    /// `n / 2 + 1` bins; the imaginary parts of the DC and Nyquist bins are ignored, since a
    /// real signal cannot have them.
    pub fn inverse(&self, spec: &[Complex32]) -> Vec<f32> {
        let Some((_, inv)) = &self.plans else {
            return Vec::new();
        };
        let mut input = inv.make_input_vec();
        let k = spec.len().min(input.len());
        input[..k].copy_from_slice(&spec[..k]);
        input[0].im = 0.0;
        if self.n % 2 == 0 {
            let last = input.len() - 1;
            input[last].im = 0.0;
        }
        let mut out = inv.make_output_vec();
        let _ = inv.process(&mut input, &mut out);
        let scale = 1.0 / self.n as f32;
        out.iter_mut().for_each(|v| *v *= scale);
        out
    }
}

/// The analysis frame size for a region of `region` samples: the largest power of two that
/// is at most `target_s × rate` samples, and small enough that `min_frames` whole frame
/// lengths fit in the region (`n ≤ region / min_frames`; at a hop of `n / 4` that leaves at
/// least `4·(min_frames − 1) + 1` frames). `min_frames` below 1 counts as 1.
///
/// Returns 0 when not even 4 samples (a hop of 1) fit.
pub fn frame_size_for(region: usize, rate: u32, target_s: f32, min_frames: f32) -> usize {
    let by_time = f64::from(target_s) * f64::from(rate);
    // `max` and `min` skip NaN: a NaN `min_frames` counts as 1, a NaN target is caught below.
    let by_region = region as f64 / f64::from(min_frames).max(1.0);
    let limit = by_time.min(by_region).min((1u64 << 30) as f64);
    if by_time.is_nan() || limit < 4.0 {
        return 0;
    }
    let mut n = 4usize;
    while ((n * 2) as f64) <= limit {
        n *= 2;
    }
    n
}

/// Hann-windowed spectra of `ch` at a hop of `n / 4`, the last frame covering
/// `ch[end − n .. end]` (so `end` is exclusive: the first sample after it), oldest first.
/// No frame starts before `first_allowed`. `end` is clamped to the channel.
///
/// Empty when `n` is 0 or not even one frame fits.
pub fn frames_ending_at(
    ch: &[f32],
    end: usize,
    n: usize,
    first_allowed: usize,
) -> Vec<Vec<Complex32>> {
    let end = end.min(ch.len());
    if n == 0 || end < n {
        return Vec::new();
    }
    let hop = (n / 4).max(1);
    let fft = Fft::new(n);
    let w = hann(n);
    let mut start = Some(end - n);
    let mut frames = Vec::new();
    while let Some(s) = start.filter(|&s| s >= first_allowed) {
        let x: Vec<f32> = ch[s..s + n].iter().zip(&w).map(|(a, b)| a * b).collect();
        frames.push(fft.forward(&x));
        start = s.checked_sub(hop);
    }
    frames.reverse();
    frames
}

/// The averaged power spectrum (Welch's method) of `ch[first..=last]`: the mean of `|X|²`
/// over Hann frames of `n` at a hop of `n / 4`, the first starting at `first`. Samples after
/// the last whole frame are left out. A region shorter than one frame gives a single
/// zero-padded frame.
///
/// `n / 2 + 1` values, all zero for an empty range; empty for `n = 0`.
pub fn welch_power(ch: &[f32], first: usize, last: usize, n: usize) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    let bins = n / 2 + 1;
    let last = last.min(ch.len().saturating_sub(1));
    if ch.is_empty() || first > last {
        return vec![0.0; bins];
    }
    let region = &ch[first..=last];
    let hop = (n / 4).max(1);
    let fft = Fft::new(n);
    let w = hann(n);
    let starts: Vec<usize> = if region.len() < n {
        vec![0]
    } else {
        (0..=region.len() - n).step_by(hop).collect()
    };
    let mut sum = vec![0.0f64; bins];
    for &s in &starts {
        let x: Vec<f32> = region[s..(s + n).min(region.len())]
            .iter()
            .zip(&w)
            .map(|(a, b)| a * b)
            .collect();
        for (acc, c) in sum.iter_mut().zip(fft.forward(&x)) {
            *acc += f64::from(c.norm_sqr());
        }
    }
    let count = starts.len() as f64;
    sum.iter().map(|&p| (p / count) as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rng::SplitMix64;

    #[test]
    fn fft_round_trips() {
        let mut r = SplitMix64::new(1);
        for n in [256, 1024] {
            let x: Vec<f32> = (0..n).map(|_| r.next_f32() * 2.0 - 1.0).collect();
            let fft = Fft::new(n);
            let spec = fft.forward(&x);
            assert_eq!(spec.len(), n / 2 + 1);
            let y = fft.inverse(&spec);
            assert_eq!(y.len(), n);
            for (a, b) in x.iter().zip(&y) {
                assert!((a - b).abs() < 1e-5, "{a} vs {b}");
            }
        }
    }

    #[test]
    fn fft_handles_odd_tiny_and_mismatched_sizes() {
        for n in [0, 1, 2, 3, 7] {
            let fft = Fft::new(n);
            let x: Vec<f32> = (0..n).map(|i| i as f32 - 1.0).collect();
            let spec = fft.forward(&x);
            assert_eq!(spec.len(), n / 2 + 1);
            let y = fft.inverse(&spec);
            assert_eq!(y.len(), n);
            for (a, b) in x.iter().zip(&y) {
                assert!((a - b).abs() < 1e-5);
            }
        }
        let fft = Fft::new(16);
        assert_eq!(fft.forward(&[1.0; 40]).len(), 9);
        assert_eq!(fft.forward(&[]).len(), 9);
        assert_eq!(fft.inverse(&[Complex32::new(1.0, 1.0); 3]).len(), 16);
    }

    #[test]
    fn hann_is_periodic() {
        let w = hann(8);
        assert_eq!(w.len(), 8);
        assert!(w[0].abs() < 1e-7);
        assert!((w[4] - 1.0).abs() < 1e-7);
        assert!((w[1] - w[7]).abs() < 1e-7);
        assert!(hann(0).is_empty());
    }

    #[test]
    fn frame_size_respects_the_target_and_the_region() {
        // 93 ms at 48 kHz is 4464 samples: 4096 fits when the region is long enough.
        assert_eq!(frame_size_for(48_000, 48_000, 0.093, 2.5), 4096);
        // 2.5 frames must fit: 10_000 / 2.5 = 4000, so 2048.
        assert_eq!(frame_size_for(10_000, 48_000, 0.093, 2.5), 2048);
        // Nothing fits.
        assert_eq!(frame_size_for(5, 48_000, 0.093, 2.5), 0);
        assert_eq!(frame_size_for(0, 0, 0.093, 2.5), 0);
        assert_eq!(frame_size_for(48_000, 48_000, f32::NAN, 2.5), 0);
        assert_eq!(frame_size_for(48_000, 48_000, 0.093, f32::NAN), 4096);
    }

    #[test]
    fn frames_end_at_the_cut_and_stay_inside_the_region() {
        let x: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.1).sin()).collect();
        let n = 64;
        let frames = frames_ending_at(&x, 900, n, 700);
        // Starts 836, 820, ..., down to >= 700: (836 - 700) / 16 + 1 = 9 frames.
        assert_eq!(frames.len(), 9);
        let fft = Fft::new(n);
        let w = hann(n);
        let last: Vec<f32> = x[836..900].iter().zip(&w).map(|(a, b)| a * b).collect();
        let want = fft.forward(&last);
        for (a, b) in frames[8].iter().zip(&want) {
            assert!((a - b).norm() < 1e-5);
        }
        assert!(frames_ending_at(&x, 50, n, 0).is_empty());
        assert!(frames_ending_at(&x, 900, 0, 0).is_empty());
        assert!(frames_ending_at(&[], 900, n, 0).is_empty());
    }

    #[test]
    fn welch_power_peaks_at_the_tone() {
        let n = 256;
        let rate = 8000.0;
        let f = 1000.0; // bin 32
        let x: Vec<f32> = (0..4000)
            .map(|i| (std::f32::consts::TAU * f * i as f32 / rate).sin())
            .collect();
        let p = welch_power(&x, 100, 3999, n);
        assert_eq!(p.len(), n / 2 + 1);
        let best = (0..p.len()).max_by(|&a, &b| p[a].total_cmp(&p[b])).unwrap();
        assert_eq!(best, 32);
        // A Hann-windowed unit sine has |X| = n/4 at its bin.
        assert!((p[32].sqrt() - n as f32 / 4.0).abs() < 0.5);
        assert!(welch_power(&[], 0, 10, n).iter().all(|&v| v == 0.0));
        assert!(welch_power(&x, 0, 10, 0).is_empty());
    }
}
