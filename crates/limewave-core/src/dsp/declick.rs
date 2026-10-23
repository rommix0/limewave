//! Click rebuilding: a short stretch of audio replaced by a prediction from both sides.
//!
//! An autoregressive model is fitted by Burg's method to the audio before the stretch and
//! run forward across it, and another is fitted to the audio after it and run backward. The
//! two predictions are cross-faded, so the result leaves the left edge on the forward model
//! and arrives at the right edge on the backward one. Everything is f64 inside, and the
//! result is deterministic.

use std::f64::consts::PI;

/// The fewest and most coefficients the model uses.
const MIN_ORDER: usize = 32;
const MAX_ORDER: usize = 256;

/// The model order for a stretch of `gap` frames: `gap / 8`, kept within 32 to 256.
pub fn order_for(gap: usize) -> usize {
    (gap / 8).clamp(MIN_ORDER, MAX_ORDER)
}

/// The real audio [`rebuild`] needs on each side of a stretch of `gap` frames: the stretch's
/// own length, or three model orders if that is more.
pub fn context_needed(gap: usize) -> usize {
    gap.max(3 * order_for(gap))
}

/// New values for `channel[first..=last]`, predicted from the audio on both sides, or `None`
/// when `first > last`, `last` is past the end, or either side has fewer than
/// [`context_needed`] frames (the left side is `first` frames, the right side is
/// `channel.len() - 1 - last`). The values at `first..=last` themselves are never read.
pub fn rebuild(channel: &[f32], first: usize, last: usize) -> Option<Vec<f32>> {
    if first > last || last >= channel.len() {
        return None;
    }
    let m = last - first + 1;
    let need = context_needed(m);
    let right = channel.len() - 1 - last;
    if first < need || right < need {
        return None;
    }
    let p = order_for(m);
    // Each side uses what it has, up to a few stretch lengths or model orders.
    let cap = (4 * m).max(8 * p);
    let left_n = first.min(cap);
    let right_n = right.min(cap);

    let left: Vec<f64> = channel[first - left_n..first]
        .iter()
        .map(|&v| f64::from(v))
        .collect();
    let fwd = predict(&left, p, m);
    let reversed: Vec<f64> = channel[last + 1..last + 1 + right_n]
        .iter()
        .rev()
        .map(|&v| f64::from(v))
        .collect();
    let bwd = predict(&reversed, p, m);

    Some(
        (0..m)
            .map(|t| {
                let w = 0.5 + 0.5 * (PI * (t + 1) as f64 / (m + 1) as f64).cos();
                // The backward run starts at the right edge, so its step j lands on frame
                // m - 1 - j.
                (w * fwd[t] + (1.0 - w) * bwd[m - 1 - t]) as f32
            })
            .collect(),
    )
}

/// Fits an order-`p` model to `x` and runs it `steps` frames beyond the end.
fn predict(x: &[f64], p: usize, steps: usize) -> Vec<f64> {
    let a = burg(x, p);
    let mut buf = x.to_vec();
    for _ in 0..steps {
        let t = buf.len();
        let v = -(1..=p).map(|k| a[k] * buf[t - k]).sum::<f64>();
        buf.push(v);
    }
    buf.split_off(x.len())
}

/// Burg's method: the coefficients `a[1..=p]` (`a[0]` is 1) with `x[t] ≈ −Σ a[k]·x[t−k]`.
/// Each stage picks the reflection coefficient that minimises the forward and backward
/// prediction errors together, so the model is always stable.
fn burg(x: &[f64], p: usize) -> Vec<f64> {
    let mut a = vec![0.0; p + 1];
    a[0] = 1.0;
    let mut f = x.to_vec();
    let mut b = x.to_vec();
    let mut prev = vec![0.0; p + 1];
    for k in 1..=p.min(x.len().saturating_sub(1)) {
        let (mut num, mut den) = (0.0, 0.0);
        for t in k..x.len() {
            num += f[t] * b[t - 1];
            den += f[t] * f[t] + b[t - 1] * b[t - 1];
        }
        let refl = if den > 0.0 { -2.0 * num / den } else { 0.0 };
        prev.copy_from_slice(&a);
        for i in 1..k {
            a[i] = prev[i] + refl * prev[k - i];
        }
        a[k] = refl;
        // Descending, so b[t - 1] is still the previous stage's when it is read.
        for t in (k..x.len()).rev() {
            let (ft, bt) = (f[t], b[t - 1]);
            f[t] = ft + refl * bt;
            b[t] = bt + refl * ft;
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rng::SplitMix64;
    use crate::testwav::sine;

    /// `err_rms / ref_rms` in decibels.
    fn db(err_rms: f64, ref_rms: f64) -> f64 {
        20.0 * (err_rms / ref_rms).log10()
    }

    fn rms(x: impl Iterator<Item = f64>) -> f64 {
        let (sum, n) = x.fold((0.0, 0usize), |(s, n), v| (s + v * v, n + 1));
        (sum / n as f64).sqrt()
    }

    #[test]
    fn a_spike_in_a_sine_is_rebuilt() {
        let clean = sine(440.0, 44_100, 8_000, 0.5);
        let mut x = clean.clone();
        x[4000] = 1.0;
        let y = rebuild(&x, 4000, 4000).unwrap();
        assert_eq!(y.len(), 1);
        let err = f64::from((y[0] - clean[4000]).abs());
        assert!(db(err, 0.5) < -40.0, "error {err}");
    }

    #[test]
    fn a_noise_burst_in_a_sine_is_rebuilt() {
        let clean = sine(440.0, 44_100, 8_000, 0.5);
        let mut x = clean.clone();
        let mut r = SplitMix64::new(5);
        for v in &mut x[4000..4088] {
            *v = 0.8 * (r.next_f32() * 2.0 - 1.0);
        }
        let y = rebuild(&x, 4000, 4087).unwrap();
        assert_eq!(y.len(), 88);
        let err = rms((0..88).map(|i| f64::from(y[i] - clean[4000 + i])));
        let reference = rms(clean.iter().map(|&v| f64::from(v)));
        assert!(db(err, reference) < -40.0, "{} dB", db(err, reference));
    }

    #[test]
    fn a_fifty_ms_gap_comes_back_at_full_level() {
        let clean = sine(220.0, 44_100, 20_000, 0.5);
        let mut x = clean.clone();
        x[8000..8000 + 2205].fill(0.0);
        let y = rebuild(&x, 8000, 8000 + 2204).unwrap();
        assert_eq!(y.len(), 2205);
        let got = rms(y.iter().map(|&v| f64::from(v)));
        let want = rms(clean[8000..10205].iter().map(|&v| f64::from(v)));
        assert!(db(got, want).abs() < 1.0, "{} dB", db(got, want));
        assert!((y[0] - clean[8000]).abs() < 0.025);
        assert!((y[2204] - clean[10204]).abs() < 0.025);
    }

    #[test]
    fn too_little_context_is_refused() {
        let x = sine(440.0, 44_100, 8_000, 0.5);
        assert_eq!(context_needed(1), 96);
        assert!(rebuild(&x, 10, 10).is_none());
        assert!(rebuild(&x, 95, 95).is_none());
        assert!(rebuild(&x, 96, 96).is_some());
        assert!(rebuild(&x, 8_000 - 11, 8_000 - 11).is_none());
        assert!(rebuild(&x, 8_000 - 97, 8_000 - 97).is_some());
        assert!(rebuild(&x, 5, 4).is_none());
        assert!(rebuild(&x, 4000, 8_000).is_none());
    }

    #[test]
    fn rebuilding_is_deterministic() {
        let mut x = sine(330.0, 44_100, 6_000, 0.4);
        x[3000..3040].fill(0.9);
        assert_eq!(rebuild(&x, 3000, 3039), rebuild(&x, 3000, 3039));
    }
}
