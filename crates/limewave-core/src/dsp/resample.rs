//! Band-limited sample-rate conversion.
//!
//! A Kaiser-windowed sinc, tabulated once and read with linear interpolation between table
//! points. The cutoff sits at the lower of the two Nyquists with a small guard band, so a
//! downward conversion removes what the new rate cannot hold instead of folding it back.
//! Edges are handled by holding the first and last frames beyond the audio, and the result
//! is deterministic: the same input and rates always give the same samples.

use std::f64::consts::PI;
use std::sync::OnceLock;

/// The lowest rate a conversion may target, in whole hertz.
pub const MIN_RATE: u32 = 1_000;

/// The highest rate a conversion may target, in whole hertz.
pub const MAX_RATE: u32 = 768_000;

/// Zero crossings of the sinc kept on each side of the centre.
const ZEROS: usize = 64;

/// Kaiser window shape: about 100 dB of stop-band rejection.
const BETA: f64 = 10.0;

/// Table points per zero crossing; between them the kernel is read linearly.
const TABLE_STEPS: usize = 1024;

/// The cutoff as a fraction of the input rate when the rate does not fall (the guard band
/// below the 0.5 Nyquist).
const CUTOFF: f64 = 0.475;

/// The number of frames `frames` input frames make when the rate changes from `from` to
/// `to`: `round(frames * to / from)`, halves rounding up. Zero frames stay zero.
pub fn resampled_len(frames: usize, from: u32, to: u32) -> usize {
    let num = frames as u128 * to as u128;
    let den = from as u128;
    ((2 * num + den) / (2 * den)) as usize
}

/// `audio` (one `Vec` per channel) converted from `from` to `to` hertz. Every output channel
/// is [`resampled_len`] long. When the rates are equal this is a plain copy.
pub fn resample(audio: &[Vec<f32>], from: u32, to: u32) -> Vec<Vec<f32>> {
    if from == to {
        return audio.to_vec();
    }
    let kernel = kernel();
    audio
        .iter()
        .map(|channel| resample_channel(channel, from, to, kernel))
        .collect()
}

/// One channel through the filter.
fn resample_channel(input: &[f32], from: u32, to: u32, kernel: &[f64]) -> Vec<f32> {
    let out_len = resampled_len(input.len(), from, to);
    if input.is_empty() {
        return vec![0.0; out_len];
    }
    // Cutoff in cycles per input sample, and the half-width of the kernel in input samples.
    let fc = CUTOFF * (to as f64 / from as f64).min(1.0);
    let half_width = ZEROS as f64 / (2.0 * fc);
    let last = input.len() as i64 - 1;
    let (from64, to64) = (from as u64, to as u64);
    (0..out_len as u64)
        .map(|n| {
            // Output frame n sits at input position ip + frac; the split keeps long files
            // from drifting.
            let scaled = n * from64;
            let ip = (scaled / to64) as i64;
            let frac = (scaled % to64) as f64 / to64 as f64;
            let x = ip as f64 + frac;
            let first = (x - half_width).ceil() as i64;
            let end = (x + half_width).floor() as i64;
            let (mut sum, mut weights) = (0.0f64, 0.0f64);
            for k in first..=end {
                // Distance from the output position to input frame k, from exact parts.
                let t = (ip - k) as f64 + frac;
                let w = kernel_at(kernel, (2.0 * fc * t).abs());
                sum += input[k.clamp(0, last) as usize] as f64 * w;
                weights += w;
            }
            (sum / weights) as f32
        })
        .collect()
}

/// The kernel table, built on first use: `g(u) = sinc(u) * kaiser(u / ZEROS)` for `u` in
/// `0..=ZEROS`, `TABLE_STEPS` points per unit, plus a spare point for interpolation.
fn kernel() -> &'static [f64] {
    static TABLE: OnceLock<Vec<f64>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let i0_beta = bessel_i0(BETA);
        (0..=ZEROS * TABLE_STEPS + 1)
            .map(|i| {
                let u = i as f64 / TABLE_STEPS as f64;
                let r = u / ZEROS as f64;
                if r >= 1.0 {
                    return 0.0;
                }
                let sinc = if u == 0.0 {
                    1.0
                } else {
                    (PI * u).sin() / (PI * u)
                };
                sinc * bessel_i0(BETA * (1.0 - r * r).sqrt()) / i0_beta
            })
            .collect()
    })
}

/// `g(u)` for `u >= 0`, linearly interpolated; zero beyond the last zero crossing.
fn kernel_at(kernel: &[f64], u: f64) -> f64 {
    let pos = u * TABLE_STEPS as f64;
    let i = pos as usize;
    if i + 1 >= kernel.len() {
        return 0.0;
    }
    let f = pos - i as f64;
    kernel[i] + (kernel[i + 1] - kernel[i]) * f
}

/// The modified Bessel function of the first kind, order zero, by its power series.
fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let (mut term, mut sum) = (1.0f64, 1.0f64);
    for k in 1..200 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// A sine of `freq` hertz, `secs` long at `rate`, peaking at `amp`. The phase is worked in
    /// f64: in f32 its rounding is itself noise at about -80 dB, which would mask the
    /// resampler's stop band.
    fn sine(freq: f32, rate: u32, secs: f32, amp: f32) -> Vec<f32> {
        let n = (secs * rate as f32) as usize;
        (0..n)
            .map(|i| (amp as f64 * (2.0 * PI * freq as f64 * i as f64 / rate as f64).sin()) as f32)
            .collect()
    }

    /// The RMS of the middle half of `x`, so the edges are left out.
    fn rms(x: &[f32]) -> f32 {
        let mid = &x[x.len() / 4..x.len() * 3 / 4];
        (mid.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / mid.len() as f64).sqrt() as f32
    }

    /// The level of one frequency in `x`, in dB, by the Goertzel recurrence. A Hann window is
    /// applied first, so a tone's own leakage stays far below the 60 dB these tests ask for.
    fn goertzel_db(x: &[f32], freq: f32, rate: u32) -> f32 {
        let n = x.len();
        let w = TAU as f64 * freq as f64 / rate as f64;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for (i, v) in x.iter().enumerate() {
            let hann = 0.5 - 0.5 * (2.0 * PI * i as f64 / (n - 1) as f64).cos();
            let s = *v as f64 * hann + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        (10.0 * power.max(1e-30).log10()) as f32
    }

    #[test]
    fn length_rounds_to_the_nearest_frame() {
        assert_eq!(resampled_len(44_100, 44_100, 31_250), 31_250);
        assert_eq!(resampled_len(3, 2, 1), 2); // 1.5 rounds up
        assert_eq!(resampled_len(0, 44_100, 8_000), 0);
    }

    #[test]
    fn a_tone_keeps_its_pitch_and_level_through_odd_rates() {
        // 1 kHz at 44.1k -> 31,250 -> 13,000 -> 96,000: still 1 kHz, level within 0.1 dB.
        let x = vec![sine(1000.0, 44_100, 0.5, 0.5)];
        let a = resample(&x, 44_100, 31_250);
        let b = resample(&a, 31_250, 13_000);
        let c = resample(&b, 13_000, 96_000);
        let want = 0.5 / 2f32.sqrt();
        assert!((20.0 * (rms(&c[0]) / want).log10()).abs() < 0.1);
        assert!(goertzel_db(&c[0], 1000.0, 96_000) > goertzel_db(&c[0], 1100.0, 96_000) + 60.0);
    }

    #[test]
    fn downsampling_removes_what_the_new_rate_cannot_hold() {
        // 10 kHz is above 6.5 kHz, the Nyquist of 13 kHz: nothing may fold back.
        let y = resample(&[sine(10_000.0, 44_100, 0.5, 0.5)], 44_100, 13_000);
        assert!(20.0 * rms(&y[0]).max(1e-12).log10() < -90.0);
    }

    #[test]
    fn the_passband_reaches_ninety_percent_of_the_lower_nyquist() {
        // 0.9 x 6.5 kHz = 5.85 kHz survives 44.1k -> 13k within 0.1 dB.
        let y = resample(&[sine(5_850.0, 44_100, 0.5, 0.5)], 44_100, 13_000);
        assert!((20.0 * (rms(&y[0]) / (0.5 / 2f32.sqrt())).log10()).abs() < 0.1);
    }

    #[test]
    fn constant_audio_stays_constant_to_the_edges() {
        let y = resample(&[vec![0.5; 1000]], 44_100, 48_000);
        assert!(y[0].iter().all(|v| (v - 0.5).abs() < 1e-4));
    }

    #[test]
    fn channels_and_determinism() {
        let x = vec![sine(440.0, 22_050, 0.1, 0.3), sine(660.0, 22_050, 0.1, 0.3)];
        let a = resample(&x, 22_050, 44_100);
        assert_eq!(a.len(), 2);
        assert_eq!(a, resample(&x, 22_050, 44_100));
        assert_eq!(resample(&x, 22_050, 22_050), x);
        assert_eq!(
            resample(&[Vec::new()], 44_100, 8_000),
            vec![Vec::<f32>::new()]
        );
    }
}
