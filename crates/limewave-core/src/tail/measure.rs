//! Measurements of a rendered tail, as v0.3 §8.1 defines them: the join and loop-wrap steps,
//! the level at the join, the steadiness of a sustain body, the envelope against a real ending
//! and the stereo correlation. The tail tests use them, and so does the `tail_lab` bench.
//!
//! Ranges are inclusive (`first..=last`) and clamped to the shortest channel. Channels are
//! combined by energy or by their largest step, never by summing samples.

use crate::dsp::envelope::{level_db, rms_db_frames};

/// Steps within this many seconds either side of a join are checked.
const JOIN_WINDOW_S: f64 = 0.002;

/// The level after the cut is measured over this many seconds.
const LEVEL_WINDOW_S: f64 = 0.050;

/// The level at the cut is the RMS of this many seconds before it (v0.3 §4.2).
const CUT_LEVEL_S: f64 = 0.020;

/// A sustain body's level is measured in frames of this many seconds.
const BODY_FRAME_S: f64 = 0.050;

/// The envelope comparison uses frames of this many seconds...
const ENVELOPE_FRAME_S: f64 = 0.010;

/// ...until both envelopes are below this level.
const ENVELOPE_FLOOR_DB: f32 = -60.0;

/// Keeps a ratio finite when its reference is silent.
const TINY: f32 = 1e-12;

/// The shortest channel's length.
fn frames(audio: &[Vec<f32>]) -> usize {
    audio.iter().map(Vec::len).min().unwrap_or(0)
}

/// Frames in `secs` seconds at `rate`, at least 1.
fn frames_in(secs: f64, rate: u32) -> usize {
    ((secs * f64::from(rate)) as usize).max(1)
}

/// The step from frame `i` to frame `i + 1`: the largest absolute difference over channels.
fn step(audio: &[Vec<f32>], i: usize, j: usize) -> f32 {
    audio
        .iter()
        .map(|ch| (ch[j] - ch[i]).abs())
        .fold(0.0, f32::max)
}

/// The 99th percentile (linearly interpolated) of the sample-to-sample steps inside
/// `first..=last`, each the largest over channels. 0 when the range holds no step.
pub fn step_percentile_99(audio: &[Vec<f32>], first: usize, last: usize) -> f32 {
    let last = last.min(frames(audio).saturating_sub(1));
    if first >= last {
        return 0.0;
    }
    let mut steps: Vec<f32> = (first..last).map(|i| step(audio, i, i + 1)).collect();
    steps.sort_unstable_by(f32::total_cmp);
    let pos = 0.99 * (steps.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(steps.len() - 1);
    let frac = (pos - lo as f64) as f32;
    steps[lo] + (steps[hi] - steps[lo]) * frac
}

/// The join score at frame `at`: the largest step within 2 ms of `at` (from `at − 2 ms` to
/// `at + 2 ms`), divided by the 99th-percentile step inside the source region. 1.5 or less
/// counts as click-free (v0.3 §8.1).
pub fn join_score(audio: &[Vec<f32>], at: usize, source: (usize, usize), rate: u32) -> f32 {
    let len = frames(audio);
    if len < 2 {
        return 0.0;
    }
    let w = frames_in(JOIN_WINDOW_S, rate);
    let lo = at.saturating_sub(w).min(len - 2);
    let hi = at.saturating_add(w).min(len - 1);
    let worst = (lo..hi).map(|i| step(audio, i, i + 1)).fold(0.0, f32::max);
    worst / (step_percentile_99(audio, source.0, source.1) + TINY)
}

/// The loop-wrap score of the inclusive loop `(start, end)`: the largest step in the 2 ms
/// before the loop end followed by the 2 ms from the loop start, as a wrap plays them, divided
/// by the 99th-percentile step inside the source region.
pub fn wrap_score(
    audio: &[Vec<f32>],
    (start, end): (usize, usize),
    source: (usize, usize),
    rate: u32,
) -> f32 {
    let len = frames(audio);
    if end >= len || start > end {
        return 0.0;
    }
    let w = frames_in(JOIN_WINDOW_S, rate);
    // The frames a wrap plays: end − w + 1 ..= end, then start .. start + w.
    let before = end.saturating_sub(w - 1).max(start);
    let order: Vec<usize> = (before..=end)
        .chain(start..(start + w).min(end + 1))
        .collect();
    let worst = order
        .windows(2)
        .map(|p| step(audio, p[0], p[1]))
        .fold(0.0, f32::max);
    worst / (step_percentile_99(audio, source.0, source.1) + TINY)
}

/// The level of the first 50 ms from `cut` (the first new frame), compared with the level at
/// the cut (the 20 ms before it), allowing for a tail that falls at `rate_db_s` dB per second
/// over those 50 ms. Positive is quieter than it should be, in dB.
pub fn level_step_db(audio: &[Vec<f32>], cut: usize, rate: u32, rate_db_s: f32) -> f32 {
    let n = frames_in(LEVEL_WINDOW_S, rate);
    let before = level_db(
        audio,
        cut.saturating_sub(frames_in(CUT_LEVEL_S, rate)),
        cut.saturating_sub(1),
    );
    let after = level_db(audio, cut, cut + n - 1);
    // The mean energy of a decay over the window, relative to its start.
    let per_frame = f64::from(rate_db_s) / f64::from(rate.max(1));
    let mean: f64 = (0..n)
        .map(|i| 10f64.powf(-per_frame * i as f64 / 10.0))
        .sum::<f64>()
        / n as f64;
    let expected = (10.0 * mean.log10()) as f32;
    before + expected - after
}

/// The 50 ms RMS levels of `first..=last` in dB, frame by frame from `first`.
pub fn body_levels_db(audio: &[Vec<f32>], first: usize, last: usize, rate: u32) -> Vec<f32> {
    rms_db_frames(audio, first, last, frames_in(BODY_FRAME_S, rate))
}

/// How far the 50 ms RMS level of `first..=last` strays from `target_db`, at worst, in dB.
pub fn level_deviation_db(
    audio: &[Vec<f32>],
    first: usize,
    last: usize,
    target_db: f32,
    rate: u32,
) -> f32 {
    body_levels_db(audio, first, last, rate)
        .iter()
        .map(|d| (d - target_db).abs())
        .fold(0.0, f32::max)
}

/// The spread of the 50 ms RMS level of `first..=last`: loudest frame minus quietest, in dB.
pub fn body_spread_db(audio: &[Vec<f32>], first: usize, last: usize, rate: u32) -> f32 {
    let levels = body_levels_db(audio, first, last, rate);
    let hi = levels.iter().copied().fold(f32::MIN, f32::max);
    let lo = levels.iter().copied().fold(f32::MAX, f32::min);
    if levels.is_empty() { 0.0 } else { hi - lo }
}

/// The mean difference in dB between the 10 ms RMS envelopes of `a` from `a_first` and `b`
/// from `b_first`, over the frames where either is above −60 dBFS. 0 when there are none.
pub fn envelope_error_db(
    a: &[Vec<f32>],
    a_first: usize,
    b: &[Vec<f32>],
    b_first: usize,
    rate: u32,
) -> f32 {
    let hop = frames_in(ENVELOPE_FRAME_S, rate);
    let ea = rms_db_frames(a, a_first, frames(a).saturating_sub(1), hop);
    let eb = rms_db_frames(b, b_first, frames(b).saturating_sub(1), hop);
    let diffs: Vec<f32> = ea
        .iter()
        .zip(&eb)
        .filter(|&(&x, &y)| x > ENVELOPE_FLOOR_DB || y > ENVELOPE_FLOOR_DB)
        .map(|(x, y)| (x - y).abs())
        .collect();
    if diffs.is_empty() {
        0.0
    } else {
        diffs.iter().sum::<f32>() / diffs.len() as f32
    }
}

/// The correlation between the first two channels over `first..=last`:
/// `Σ l·r / √(Σ l² · Σ r²)`. 1 for mono, and 0 when either channel is silent.
pub fn channel_correlation(audio: &[Vec<f32>], first: usize, last: usize) -> f32 {
    let [l, r, ..] = audio else {
        return 1.0;
    };
    let last = last.min(frames(audio).saturating_sub(1));
    if first > last || frames(audio) == 0 {
        return 0.0;
    }
    let (mut lr, mut ll, mut rr) = (0.0f64, 0.0f64, 0.0f64);
    for i in first..=last {
        let (x, y) = (f64::from(l[i]), f64::from(r[i]));
        lr += x * y;
        ll += x * x;
        rr += y * y;
    }
    let den = (ll * rr).sqrt();
    if den > 0.0 { (lr / den) as f32 } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_known_signals() {
        // A sawtooth: steps of 0.01, and a drop of 0.99 every 100 frames (1 % of the steps).
        let x: Vec<f32> = (0..1000).map(|i| (i % 100) as f32 * 0.01).collect();
        let audio = vec![x];
        let p = step_percentile_99(&audio, 0, 999);
        assert!(p > 0.0 && p < 0.1, "{p}");
        // The first 100 frames hold only small steps, so a drop scores 99 against them.
        let s = join_score(&audio, 300, (0, 99), 1000);
        assert!(s > 1.5, "a wrap step is a click: {s}");
        let s = join_score(&audio, 350, (0, 99), 1000);
        assert!(s <= 1.01, "{s}");

        // Stereo correlation of a channel with itself, its negative and silence.
        let l: Vec<f32> = (0..500).map(|i| (i as f32 * 0.3).sin()).collect();
        let r: Vec<f32> = l.iter().map(|v| -v).collect();
        assert!((channel_correlation(&[l.clone(), l.clone()], 0, 499) - 1.0).abs() < 1e-6);
        assert!((channel_correlation(&[l.clone(), r], 0, 499) + 1.0).abs() < 1e-6);
        assert_eq!(
            channel_correlation(&[l.clone(), vec![0.0; 500]], 0, 499),
            0.0
        );
        assert_eq!(channel_correlation(std::slice::from_ref(&l), 0, 499), 1.0);

        // A steady sine has no level step and no spread; envelopes agree with themselves.
        let rate = 8000;
        // 16 frames a period, so every window holds whole periods.
        let s: Vec<f32> = (0..8000)
            .map(|i| (i as f32 * std::f32::consts::TAU / 16.0).sin() * 0.5)
            .collect();
        let audio = vec![s];
        assert!(level_step_db(&audio, 4000, rate, 0.0).abs() < 0.1);
        assert!(body_spread_db(&audio, 0, 7999, rate) < 0.1);
        assert!(level_deviation_db(&audio, 0, 7999, -9.03, rate) < 0.1);
        assert!(envelope_error_db(&audio, 0, &audio, 0, rate) < 1e-6);
        assert!(wrap_score(&audio, (0, 7999), (0, 7999), rate).is_finite());
    }

    #[test]
    fn measures_handle_degenerate_input() {
        let empty: Vec<Vec<f32>> = vec![];
        assert_eq!(step_percentile_99(&empty, 0, 10), 0.0);
        assert_eq!(join_score(&empty, 0, (0, 10), 8000), 0.0);
        assert_eq!(wrap_score(&empty, (0, 10), (0, 10), 8000), 0.0);
        assert!(level_step_db(&empty, 0, 8000, 10.0).is_finite());
        assert_eq!(body_spread_db(&empty, 0, 10, 8000), 0.0);
        assert_eq!(envelope_error_db(&empty, 0, &empty, 0, 8000), 0.0);
        let one = vec![vec![0.5f32]];
        assert_eq!(join_score(&one, 0, (0, 0), 8000), 0.0);
        assert_eq!(channel_correlation(&[vec![], vec![]], 0, 10), 0.0);
    }
}
