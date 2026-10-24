//! Finding candidate loop points: where the sound near a fixed loop point is echoed elsewhere.
//!
//! A loop `s..=e` plays on from `x[e]` to `x[s]`, so it joins well where the audio around
//! `e + 1` resembles the audio around `s`. With one point kept, the other is found by
//! correlating the audio around the kept point with the audio around every possible position
//! of the other, and measuring the best few.

use std::sync::atomic::{AtomicBool, Ordering};

use realfft::RealFftPlanner;
use realfft::num_complex::Complex64;

use super::measure::{natural_change, quality, score, seam_measures, shortest};
use super::{FinderError, LoopCandidate};

/// How many frames either side of the kept point the reference spans, at most.
const CONTEXT: usize = 1_024;

/// The most positions [`starts_for_end`] and [`ends_for_start`] return.
const MAX_PICKS: usize = 48;

/// The closest two returned positions may be, in seconds.
const MIN_GAP_S: f64 = 0.050;

/// What a search needs besides the audio.
pub(super) struct Job<'a> {
    /// The sample rate.
    pub rate: u32,
    /// The first and last frame of the range (inclusive).
    pub range: (usize, usize),
    /// The shortest loop, at least 2.
    pub min_len: usize,
    /// The longest loop.
    pub max_len: Option<usize>,
    /// The period of the range, copied into every candidate.
    pub period: Option<f32>,
    /// Set to stop the search.
    pub cancel: &'a AtomicBool,
}

/// Candidate loops of `audio` that end at `end`, measured as forward loops and unsorted. Their
/// scores are the plain seam scores.
pub(super) fn kept_end(
    audio: &[Vec<f32>],
    job: &Job,
    end: usize,
) -> Result<Vec<LoopCandidate>, FinderError> {
    let latest = (end + 1).saturating_sub(job.min_len);
    let earliest = job.max_len.map_or(job.range.0, |m| {
        job.range.0.max((end + 1).saturating_sub(m))
    });
    let starts = if earliest <= latest {
        starts_for_end(audio, job.rate, end, (earliest, latest))
    } else {
        Vec::new()
    };
    measure_all(audio, job, starts.into_iter().map(|s| (s, end)))
}

/// Candidate loops of `audio` that start at `start`, measured as forward loops and unsorted.
/// Their scores are the plain seam scores.
pub(super) fn kept_start(
    audio: &[Vec<f32>],
    job: &Job,
    start: usize,
) -> Result<Vec<LoopCandidate>, FinderError> {
    let earliest = start + job.min_len - 1;
    let latest = job
        .max_len
        .map_or(job.range.1, |m| job.range.1.min(start + m - 1));
    let ends = if earliest <= latest {
        ends_for_start(audio, job.rate, start, (earliest, latest))
    } else {
        Vec::new()
    };
    measure_all(audio, job, ends.into_iter().map(|e| (start, e)))
}

/// Measures each `(start, end)` pair, checking `cancel` before each one.
fn measure_all(
    audio: &[Vec<f32>],
    job: &Job,
    pairs: impl Iterator<Item = (usize, usize)>,
) -> Result<Vec<LoopCandidate>, FinderError> {
    let natural = natural_change(audio, job.range.0, job.range.1, job.rate);
    let mut out = Vec::new();
    for (s, e) in pairs {
        if job.cancel.load(Ordering::Relaxed) {
            return Err(FinderError::Cancelled);
        }
        let measures = seam_measures(audio, s, e, job.rate, &natural);
        out.push(LoopCandidate {
            loop_start: s,
            loop_end: e,
            score: score(&measures),
            quality: quality(&measures),
            measures,
            period: job.period,
        });
    }
    Ok(out)
}

/// The loop starts in `span` (inclusive) whose surroundings best match those of the join after
/// `end`: up to 48 local maxima of the correlation, at least 50 ms apart, in order of position.
///
/// The reference is up to 1,024 frames before `end + 1` and as many after it where the file has
/// them.
pub(super) fn starts_for_end(
    audio: &[Vec<f32>],
    rate: u32,
    end: usize,
    span: (usize, usize),
) -> Vec<usize> {
    best_matches(audio, rate, end + 1, span)
}

/// The loop ends in `span` (inclusive) that best join back to `start`: the mirror image of
/// [`starts_for_end`], whose positions are those of the frame after each end.
pub(super) fn ends_for_start(
    audio: &[Vec<f32>],
    rate: u32,
    start: usize,
    span: (usize, usize),
) -> Vec<usize> {
    best_matches(audio, rate, start, (span.0 + 1, span.1 + 1))
        .into_iter()
        .map(|p| p - 1)
        .collect()
}

/// The positions in `span` whose surroundings best match those of `anchor`.
fn best_matches(audio: &[Vec<f32>], rate: u32, anchor: usize, span: (usize, usize)) -> Vec<usize> {
    let corr = correlation(audio, anchor, span);
    let gap = ((MIN_GAP_S * f64::from(rate)) as usize).max(1);
    let mut peaks: Vec<usize> = (0..corr.len())
        .filter(|&i| {
            let left = i == 0 || corr[i] >= corr[i - 1];
            let right = i + 1 == corr.len() || corr[i] > corr[i + 1];
            left && right
        })
        .collect();
    // Best first; equal values keep the earlier position.
    peaks.sort_by(|&a, &b| corr[b].total_cmp(&corr[a]).then(a.cmp(&b)));
    let mut kept: Vec<usize> = Vec::new();
    for i in peaks {
        if kept.len() == MAX_PICKS {
            break;
        }
        if kept.iter().all(|&k| k.abs_diff(i) >= gap) {
            kept.push(i);
        }
    }
    kept.sort_unstable();
    kept.into_iter().map(|i| span.0 + i).collect()
}

/// For each position `p` in `span`, how well the audio around `p` matches the audio around
/// `anchor`: the normalised correlation of the windows `[p − B, p + A)` and
/// `[anchor − B, anchor + A)`, summed over channels. `B` and `A` are at most [`CONTEXT`] and
/// are cut to what the file has around the anchor.
///
/// Where a window runs off the audio, only the part inside is compared and the value is scaled
/// by the fraction that was, so a near-empty overlap cannot score highly.
fn correlation(audio: &[Vec<f32>], anchor: usize, span: (usize, usize)) -> Vec<f64> {
    let len = shortest(audio);
    let count = span.1 + 1 - span.0;
    let before = CONTEXT.min(anchor);
    let after = CONTEXT.min(len.saturating_sub(anchor));
    let width = before + after;
    if width == 0 || count == 0 {
        return vec![0.0; count];
    }
    // The audio under every window, zero outside the file, from `span.0 − before`.
    let origin = span.0 as isize - before as isize;
    let seg_len = count + width - 1;
    let size = (seg_len + width).next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let (fwd, inv) = (
        planner.plan_fft_forward(size),
        planner.plan_fft_inverse(size),
    );
    let mut acc = vec![Complex64::new(0.0, 0.0); size / 2 + 1];
    // Prefix energies, summed over channels.
    let mut ref_prefix = vec![0.0f64; width + 1];
    let mut seg_prefix = vec![0.0f64; seg_len + 1];
    for ch in audio {
        let mut reference = fwd.make_input_vec();
        for (j, r) in reference[..width].iter_mut().enumerate() {
            *r = f64::from(ch[anchor - before + j]);
            ref_prefix[j + 1] += *r * *r;
        }
        let mut seg = fwd.make_input_vec();
        for (k, v) in seg[..seg_len].iter_mut().enumerate() {
            let at = origin + k as isize;
            if (0..len as isize).contains(&at) {
                *v = f64::from(ch[at as usize]);
            }
            seg_prefix[k + 1] += *v * *v;
        }
        let (mut fr, mut fs) = (fwd.make_output_vec(), fwd.make_output_vec());
        let _ = fwd.process(&mut reference, &mut fr);
        let _ = fwd.process(&mut seg, &mut fs);
        for ((a, r), s) in acc.iter_mut().zip(&fr).zip(&fs) {
            *a += r.conj() * s;
        }
    }
    let mut raw = inv.make_output_vec();
    let _ = inv.process(&mut acc, &mut raw);
    for i in 0..width {
        ref_prefix[i + 1] += ref_prefix[i];
    }
    for i in 0..seg_len {
        seg_prefix[i + 1] += seg_prefix[i];
    }
    (0..count)
        .map(|i| {
            let p = span.0 + i;
            // The part of the window inside the audio.
            let lo = before.saturating_sub(p);
            let hi = width.min((len + before).saturating_sub(p));
            if hi <= lo {
                return 0.0;
            }
            let e_ref = ref_prefix[hi] - ref_prefix[lo];
            let e_seg = seg_prefix[i + hi] - seg_prefix[i + lo];
            let den = (e_ref * e_seg).sqrt();
            if den <= 0.0 {
                return 0.0;
            }
            (raw[i] / size as f64 / den).clamp(-1.0, 1.0) * (hi - lo) as f64 / width as f64
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{RATE, TILE, faded_repeat};
    use super::*;

    #[test]
    fn starts_include_every_whole_tile_back() {
        let a = faded_repeat();
        let end = a[0].len() - 11;
        let starts = starts_for_end(&a, RATE, end, (0, end + 1 - 4_410));
        assert!(starts.len() <= MAX_PICKS);
        for k in 1..=3 {
            assert!(
                starts.contains(&(end + 1 - k * TILE)),
                "k = {k}: {starts:?}"
            );
        }
        let gap = (MIN_GAP_S * f64::from(RATE)) as usize;
        assert!(starts.windows(2).all(|w| w[1] - w[0] >= gap), "{starts:?}");
    }

    #[test]
    fn ends_mirror_starts() {
        let a = faded_repeat();
        let start = 20_000;
        let ends = ends_for_start(&a, RATE, start, (start + 4_409, a[0].len() - 1));
        for k in 1..=3 {
            assert!(ends.contains(&(start + k * TILE - 1)), "k = {k}: {ends:?}");
        }
    }

    #[test]
    fn a_window_off_the_audio_scores_low() {
        let a = faded_repeat();
        // Positions 0..=3 overlap the reference's own audio by under a thousand frames.
        let c = correlation(&a, 60_000, (0, 3));
        assert!(c.iter().all(|v| v.abs() < 0.6), "{c:?}");
    }
}
