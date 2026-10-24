//! Finding candidate loop points: where the sound near a fixed loop point is echoed elsewhere.
//!
//! A loop `s..=e` plays on from `x[e]` to `x[s]`, so it joins well where the audio around
//! `e + 1` resembles the audio around `s`. With one point kept, the other is found by
//! correlating the audio around the kept point with the audio around every possible position
//! of the other, and measuring the best few. With neither kept, a coarse comparison of band
//! levels finds the pairs of places that sound alike, and the same correlation refines each.

use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};

use realfft::RealFftPlanner;
use realfft::num_complex::Complex64;

use super::measure::{
    Bands, TONE_RANGE_DB, natural_change, quality, rms_diff_above, score, seam_measures, shortest,
    tone_frame,
};
use super::{FinderError, LoopCandidate, MAX_SEARCH_SECONDS};

/// How many frames either side of the kept point the reference spans, at most.
const CONTEXT: usize = 1_024;

/// The most positions [`starts_for_end`] and [`ends_for_start`] return.
const MAX_PICKS: usize = 48;

/// The closest two returned positions may be, in seconds.
const MIN_GAP_S: f64 = 0.050;

/// The coarse frames of [`whole_range`] step by a tone frame over this.
const HOPS_PER_FRAME: usize = 4;

/// How many coarse frames either side of the seam [`whole_range`] compares as well.
const SEAM_FRAMES: usize = 4;

/// Two coarse pairs within this many frames of each other in start and in lag are the same
/// pair.
const DISTINCT_FRAMES: usize = 8;

/// The longest loop a search proposes, in frames: [`MAX_SEARCH_SECONDS`] at `rate`.
fn max_span(rate: u32) -> usize {
    (MAX_SEARCH_SECONDS * f64::from(rate)) as usize
}

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
/// scores are the plain seam scores. The loops are at most 60 s long, so a kept end far past
/// the range searches only the last 60 s before it.
pub(super) fn kept_end(
    audio: &[Vec<f32>],
    job: &Job,
    end: usize,
) -> Result<Vec<LoopCandidate>, FinderError> {
    let latest = (end + 1).saturating_sub(job.min_len);
    // No more than the last 60 s before the end, whatever the range holds.
    let reach = job
        .max_len
        .map_or(max_span(job.rate), |m| m.min(max_span(job.rate)));
    let earliest = job.range.0.max((end + 1).saturating_sub(reach));
    let starts = if earliest <= latest {
        starts_for_end(audio, job.rate, end, (earliest, latest))
    } else {
        Vec::new()
    };
    measure_all(audio, job, starts.into_iter().map(|s| (s, end)))
}

/// Candidate loops of `audio` that start at `start`, measured as forward loops and unsorted.
/// Their scores are the plain seam scores. The loops are at most 60 s long.
pub(super) fn kept_start(
    audio: &[Vec<f32>],
    job: &Job,
    start: usize,
) -> Result<Vec<LoopCandidate>, FinderError> {
    let earliest = start.saturating_add(job.min_len).saturating_sub(1);
    // No more than the first 60 s after the start, whatever the range holds.
    let reach = job
        .max_len
        .map_or(max_span(job.rate), |m| m.min(max_span(job.rate)));
    let latest = job
        .range
        .1
        .min(start.saturating_add(reach).saturating_sub(1));
    let ends = if earliest <= latest {
        ends_for_start(audio, job.rate, start, (earliest, latest))
    } else {
        Vec::new()
    };
    measure_all(audio, job, ends.into_iter().map(|e| (start, e)))
}

/// Candidate loops of `audio` with both points inside the range, measured as forward loops and
/// unsorted. Their scores are the plain seam scores.
///
/// 1. The range becomes coarse frames, a tone frame long and a quarter of one apart, each
///    described by its band levels.
/// 2. Every pair of frames `(i, i + L)` with `L` hops within the length limits is compared: the
///    RMS band-level difference, averaged over the frames up to 4 either side of the seam. The
///    walk goes one lag diagonal at a time, so memory grows with the frame count.
/// 3. The best 48 pairs, each more than 8 frames from the others in start or in lag, are
///    refined: the end is kept at `(i + L)·hop − 1` and the start is the best match within a
///    hop of `i·hop`, by the same correlation as [`starts_for_end`].
///
/// # Errors
///
/// [`FinderError::Cancelled`] when `cancel` is set; it is checked once per lag diagonal.
pub(super) fn whole_range(
    audio: &[Vec<f32>],
    job: &Job,
) -> Result<Vec<LoopCandidate>, FinderError> {
    let (first, last) = job.range;
    let len = shortest(audio);
    let n = tone_frame(job.rate);
    let hop = (n / HOPS_PER_FRAME).max(1);
    let range_len = last + 1 - first;
    // Frame j starts at `first + j·hop`. It must fit in the audio, and the loop ending just
    // before it, at `first + j·hop − 1`, must end inside the range.
    let frames = (0..)
        .take_while(|&j| j * hop <= range_len && first + j * hop + n <= len)
        .count();
    let bands = Bands::new(n, job.rate);
    let levels: Vec<Vec<f32>> = (0..frames)
        .map(|j| bands.levels(audio, first + j * hop..first + j * hop + n, false))
        .collect();
    let peaks: Vec<f32> = levels
        .iter()
        .map(|l| l.iter().copied().fold(f32::MIN, f32::max))
        .collect();
    let longest = job
        .max_len
        .unwrap_or(usize::MAX)
        .min(max_span(job.rate))
        .min(range_len);
    let shortest_lag = job.min_len.div_ceil(hop).max(1);
    let longest_lag = (longest / hop).min(frames.saturating_sub(1));
    let mut best = BestPairs::new();
    let mut diffs = Vec::with_capacity(frames);
    let mut sums = Vec::with_capacity(frames + 1);
    for lag in shortest_lag..=longest_lag {
        if job.cancel.load(Ordering::Relaxed) {
            return Err(FinderError::Cancelled);
        }
        let cells = frames - lag;
        diffs.clear();
        diffs.extend((0..cells).map(|i| {
            let floor = peaks[i].max(peaks[i + lag]) - TONE_RANGE_DB;
            rms_diff_above(&levels[i], &levels[i + lag], floor)
        }));
        // A running sum along the diagonal, so each average over the seam's frames is O(1).
        sums.clear();
        sums.push(0.0f64);
        for (k, &d) in diffs.iter().enumerate() {
            sums.push(sums[k] + f64::from(d));
        }
        for i in 0..cells {
            let lo = i.saturating_sub(SEAM_FRAMES);
            let hi = (i + SEAM_FRAMES).min(cells - 1);
            let distance = (sums[hi + 1] - sums[lo]) / (hi + 1 - lo) as f64;
            best.offer(Pair {
                distance: distance as f32,
                start: i,
                lag,
            });
        }
    }
    let mut pairs: Vec<(usize, usize)> = best
        .distinct()
        .into_iter()
        .filter_map(|p| {
            let end = first + (p.start + p.lag) * hop - 1;
            let around = first + p.start * hop;
            let lo = around
                .saturating_sub(hop)
                .max(first)
                .max((end + 1).saturating_sub(longest));
            let hi = (around + hop).min((end + 1).saturating_sub(job.min_len));
            (lo <= hi).then(|| (best_start(audio, end, (lo, hi)), end))
        })
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    measure_all(audio, job, pairs.into_iter())
}

/// The start in `span` (inclusive) whose surroundings best match those of the join after
/// `end`; ties go to the earlier start.
fn best_start(audio: &[Vec<f32>], end: usize, span: (usize, usize)) -> usize {
    let corr = correlation(audio, end + 1, span);
    let best = (0..corr.len()).fold(0, |b, i| if corr[i] > corr[b] { i } else { b });
    span.0 + best
}

/// A coarse pair of frames: the loop starts at frame `start`, and the frame after its end is
/// `start + lag`.
#[derive(Clone, Copy, Debug)]
struct Pair {
    /// The averaged band-level difference across the seam, in dB. Lower is better.
    distance: f32,
    /// The start frame.
    start: usize,
    /// The loop length in frames.
    lag: usize,
}

impl Pair {
    /// Whether `self` and `other` are too close in both start and lag to count as distinct.
    fn near(&self, other: &Pair) -> bool {
        self.start.abs_diff(other.start) <= DISTINCT_FRAMES
            && self.lag.abs_diff(other.lag) <= DISTINCT_FRAMES
    }
}

// Pairs order by distance, then the earlier start, then the shorter lag: the finder's
// tie-break, so the result never depends on the order the pairs were seen in.
impl Ord for Pair {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then(self.start.cmp(&other.start))
            .then(self.lag.cmp(&other.lag))
    }
}

impl PartialOrd for Pair {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Pair {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Pair {}

/// How many pairs [`BestPairs`] keeps: enough for [`MAX_PICKS`] distinct ones.
const POOL: usize = MAX_PICKS * (2 * DISTINCT_FRAMES + 1) * (2 * DISTINCT_FRAMES + 1);

/// The best coarse pairs seen so far, in a fixed amount of memory.
///
/// Picking distinct pairs greedily from the best down, each pick rules out at most the 17 by 17
/// pairs near it. So the 48 picks all lie among the best 48 times 17² pairs, and keeping only
/// those gives exactly the picks that a list of every pair would.
struct BestPairs {
    /// The kept pairs, the worst on top.
    heap: BinaryHeap<Pair>,
}

impl BestPairs {
    /// An empty pool.
    fn new() -> BestPairs {
        BestPairs {
            heap: BinaryHeap::with_capacity(POOL + 1),
        }
    }

    /// Keeps `p` if it is among the best [`POOL`] pairs so far.
    fn offer(&mut self, p: Pair) {
        if self.heap.len() < POOL {
            self.heap.push(p);
        } else if self.heap.peek().is_some_and(|worst| p < *worst) {
            self.heap.pop();
            self.heap.push(p);
        }
    }

    /// Up to [`MAX_PICKS`] pairs, best first, none near a better one already picked.
    fn distinct(self) -> Vec<Pair> {
        let mut picked: Vec<Pair> = Vec::new();
        for p in self.heap.into_sorted_vec() {
            if picked.len() == MAX_PICKS {
                break;
            }
            if !picked.iter().any(|q| q.near(&p)) {
                picked.push(p);
            }
        }
        picked
    }
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
