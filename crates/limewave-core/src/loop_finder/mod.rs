//! The automatic loop finder (v0.8): searches a sound for the best loop points.
//!
//! This module holds the shared constants, the error type and the entry point [`find_loops`].
//! [`steady`] picks the part of the sound worth searching, [`measure`] judges a candidate loop,
//! and `search` finds the candidates; this module ranks them.

pub mod lab;
pub mod measure;
mod search;
pub mod steady;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::dsp::pitch;

pub use measure::{
    Natural, Quality, SeamMeasures, natural_change, quality, score, seam_measures, tone_frame,
};

/// The longest range the finder will search, in seconds. A longer steady part keeps its last
/// 60 s; a longer selection is refused.
pub const MAX_SEARCH_SECONDS: f64 = 60.0;

/// The most loop candidates the finder returns.
pub const MAX_CANDIDATES: usize = 8;

/// The default shortest loop the finder will propose, in seconds.
pub const DEFAULT_MIN_SECONDS: f64 = 0.1;

/// Why the finder could not produce loops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FinderError {
    /// The range to search is silent.
    Silent,
    /// The range to search is empty or too short to hold a loop.
    RangeTooShort,
    /// The selection is longer than [`MAX_SEARCH_SECONDS`].
    RangeTooLong,
    /// The settings are not usable (for example a minimum length longer than the maximum).
    BadSettings,
    /// The search was cancelled before it finished.
    Cancelled,
}

/// Which way the loop plays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    /// From the loop start to the loop end, then back to the start.
    Forward,
    /// From the loop end back to the loop start, then to the end again.
    Backward,
}

/// What the finder is asked to do. Frame positions are inclusive.
#[derive(Clone, Debug, PartialEq)]
pub struct FinderSettings {
    /// The first and last frame to search (from [`steady::search_range`]).
    pub range: (usize, usize),
    /// A loop end to keep fixed, or `None` to search both points. It may lie past `range.1`.
    pub keep_end: Option<usize>,
    /// The shortest loop to propose, in frames.
    pub min_len: usize,
    /// The longest loop to propose, in frames, or `None` for no limit.
    pub max_len: Option<usize>,
    /// How much to favour long loops: 0 favours short, 1 favours long, 0.5 is neutral.
    pub preference: f32,
    /// Which way the loop plays.
    pub direction: Direction,
    /// The most candidates to return.
    pub max_candidates: usize,
}

/// A loop the finder proposes.
#[derive(Clone, Debug, PartialEq)]
pub struct LoopCandidate {
    /// The first frame of the loop.
    pub loop_start: usize,
    /// The last frame of the loop.
    pub loop_end: usize,
    /// The ranking score: the seam [`score`] less the credit for the length preference. Lower
    /// is better.
    pub score: f32,
    /// The word for the seam.
    pub quality: Quality,
    /// The measures behind the word.
    pub measures: SeamMeasures,
    /// The period of the searched range in frames, when it is tonal. The same for every
    /// candidate of one search.
    pub period: Option<f32>,
}

impl LoopCandidate {
    /// The loop's length in frames.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.loop_end + 1 - self.loop_start
    }

    /// The length divided by the period: how many cycles the loop holds. `None` when the range
    /// is not tonal.
    pub fn cycles(&self) -> Option<f32> {
        self.period.map(|p| self.len() as f32 / p)
    }
}

/// How strongly a range must repeat for the finder to report a period.
const PERIOD_THRESHOLD: f32 = 0.6;

/// Finds loop candidates for `settings`, best first.
///
/// - A `keep_end` fixes the loop end and searches for the start. Without one both points are
///   searched for inside the range.
/// - `cancel` is checked between stages and between candidates; once set the result is
///   [`FinderError::Cancelled`].
///
/// # Errors
///
/// - [`FinderError::BadSettings`]: `min_len` is longer than `max_len`, there are no candidates
///   to return, `preference` is not a number, or `keep_end` lies outside audio that is not
///   empty.
/// - [`FinderError::RangeTooLong`]: the range is longer than [`MAX_SEARCH_SECONDS`].
/// - [`FinderError::RangeTooShort`]: the audio is empty, or the range, or the span up to a kept
///   end, is shorter than `min_len` or one tone frame.
/// - [`FinderError::Silent`]: the range is silent.
/// - [`FinderError::Cancelled`]: `cancel` was set.
pub fn find_loops(
    audio: &[Vec<f32>],
    rate: u32,
    settings: &FinderSettings,
    cancel: &AtomicBool,
) -> Result<Vec<LoopCandidate>, FinderError> {
    let len = measure::shortest(audio);
    let max_len = settings.max_len.unwrap_or(usize::MAX);
    if settings.min_len > max_len
        || settings.max_candidates == 0
        || !settings.preference.is_finite()
        || (len > 0 && settings.keep_end.is_some_and(|e| e >= len))
    {
        return Err(FinderError::BadSettings);
    }
    let first = settings.range.0;
    let last = settings.range.1.min(len.saturating_sub(1));
    if len == 0 || first > last {
        return Err(FinderError::RangeTooShort);
    }
    let range_len = last + 1 - first;
    if range_len as f64 > MAX_SEARCH_SECONDS * f64::from(rate) {
        return Err(FinderError::RangeTooLong);
    }
    // A loop is at least two frames, and no span shorter than a tone frame can be judged.
    let min_len = settings.min_len.max(2);
    let needed = min_len.max(tone_frame(rate));
    let span = settings
        .keep_end
        .map_or(range_len, |e| (e + 1).saturating_sub(first));
    if range_len < needed || span < needed {
        return Err(FinderError::RangeTooShort);
    }
    if steady::range_is_silent(audio, rate, first, last) {
        return Err(FinderError::Silent);
    }
    let stopped = || cancel.load(Ordering::Relaxed);
    if stopped() {
        return Err(FinderError::Cancelled);
    }
    let period = pitch::character(audio, first, last, rate, PERIOD_THRESHOLD).period;
    let job = search::Job {
        rate,
        range: (first, last),
        min_len,
        max_len: settings.max_len,
        period,
        cancel,
    };
    let found = match settings.direction {
        Direction::Forward => match settings.keep_end {
            None => search::whole_range(audio, &job)?,
            Some(end) => search::kept_end(audio, &job, end)?,
        },
        Direction::Backward => {
            // A backward loop is a forward loop of the reversed audio: index `i` becomes
            // `len − 1 − i`, so a kept end is a kept start.
            let flip = |i: usize| len - 1 - i;
            let reversed: Vec<Vec<f32>> = audio
                .iter()
                .map(|ch| ch[..len].iter().rev().copied().collect())
                .collect();
            let mirror = search::Job {
                range: (flip(last), flip(first)),
                ..job
            };
            let mut found = match settings.keep_end {
                None => search::whole_range(&reversed, &mirror)?,
                Some(end) => search::kept_start(&reversed, &mirror, flip(end))?,
            };
            for c in &mut found {
                (c.loop_start, c.loop_end) = (flip(c.loop_end), flip(c.loop_start));
            }
            found
        }
    };
    if stopped() {
        return Err(FinderError::Cancelled);
    }
    Ok(rank(
        found,
        settings.preference.clamp(0.0, 1.0),
        range_len,
        settings.max_candidates,
        rate,
    ))
}

/// How far before the range, and after the range or a kept end, [`find_loops`] reads the audio,
/// in frames: a tone frame and the correlation context, and at least 8,192. A copy of the audio
/// from `range.0 − margin` to `max(range.1, keep_end) + margin`, with the frame positions
/// shifted to match, gives the same candidates as the whole audio.
pub fn context_margin(rate: u32) -> usize {
    (2 * tone_frame(rate) + search::CONTEXT).max(8_192)
}

/// The most the length preference can change a score, at full preference and a loop as long as
/// the range.
const LENGTH_CREDIT: f32 = 0.15;

/// How far a length may be from a whole multiple of another and still count as one.
const MULTIPLE_TOLERANCE: f64 = 0.001;

/// Two loops of lengths within this fraction of each other, starting within [`SAME_START_S`],
/// are the same loop.
const SAME_LENGTH: f64 = 0.01;

/// Two loops starting within this many seconds, of near enough the same length, are the same
/// loop.
const SAME_START_S: f64 = 0.050;

/// Ranks candidates best first and drops the redundant ones.
///
/// - The score becomes `score − 0.15 · (preference − 0.5) · 2 · (len / range_len)`, so 0.5 is
///   neutral.
/// - Candidates sort by quality word first (Seamless, Good, Fair, Rough), then ascending by
///   score, so the preference only orders seams of the same word. Ties go to the earlier
///   start, then the shorter length.
/// - A candidate whose length is a whole multiple (two or more) of a [`Quality::Seamless`] or
///   [`Quality::Good`] candidate's, ranked above that base with a word no better, moves to
///   directly after it (whatever the preference). A multiple only ever moves down: one already
///   below its base keeps its place, and a Fair or Rough base pulls nothing below it.
/// - Of two candidates that are the same loop (lengths within 1 % and either starts within
///   50 ms or both [`Quality::Seamless`]) only the better is kept.
/// - Then the list is cut to `keep`.
fn rank(
    mut found: Vec<LoopCandidate>,
    preference: f32,
    range_len: usize,
    keep: usize,
    rate: u32,
) -> Vec<LoopCandidate> {
    for c in &mut found {
        let share = (c.len() as f32 / range_len as f32).min(1.0);
        c.score -= LENGTH_CREDIT * (preference - 0.5) * 2.0 * share;
    }
    found.sort_by(|a, b| {
        a.quality
            .cmp(&b.quality)
            .then(a.score.total_cmp(&b.score))
            .then(a.loop_start.cmp(&b.loop_start))
            .then(a.len().cmp(&b.len()))
    });
    let found = after_their_bases(found);
    let near = (SAME_START_S * f64::from(rate)) as usize;
    let mut kept: Vec<LoopCandidate> = Vec::new();
    for c in found {
        if kept.len() == keep {
            break;
        }
        if !kept.iter().any(|k| same_loop(k, &c, near)) {
            kept.push(c);
        }
    }
    kept
}

/// Whether `a` and `b` are the same loop for ranking, with starts within `near` frames.
fn same_loop(a: &LoopCandidate, b: &LoopCandidate, near: usize) -> bool {
    let (la, lb) = (a.len() as f64, b.len() as f64);
    let length_close = (la - lb).abs() < SAME_LENGTH * la.max(lb);
    let start_close = a.loop_start.abs_diff(b.loop_start) < near;
    let both_seamless = a.quality == Quality::Seamless && b.quality == Quality::Seamless;
    length_close && (start_close || both_seamless)
}

/// Whether `long` is `k ≥ 2` times as long as `short`, give or take 0.1 %.
fn is_multiple(long: usize, short: usize) -> bool {
    let k = (long as f64 / short as f64).round();
    k >= 2.0 && (long as f64 - k * short as f64).abs() <= MULTIPLE_TOLERANCE * long as f64
}

/// `sorted` with each whole multiple of another candidate that ranks above its base moved to
/// directly after that base. The base is the first candidate, in order, it is a multiple of
/// whose [`Quality`] word is no worse than its own and is [`Quality::Seamless`] or
/// [`Quality::Good`]; a multiple already below that base keeps its place.
///
/// - The words are compared, not the scores, so neither the preference nor unhearable
///   differences between two passes of the same repeat can separate them.
/// - Only a genuine repeat is a base: a Fair or Rough loop is not one, so a multiple of its length
///   is a different loop that happens to be that long, and keeps its place.
/// - A multiple only ever moves down, so it never passes a candidate it was below.
/// - Several multiples of one base keep their order, each followed by its own multiples.
fn after_their_bases(sorted: Vec<LoopCandidate>) -> Vec<LoopCandidate> {
    let base_of: Vec<Option<usize>> = (0..sorted.len())
        .map(|i| {
            (0..sorted.len())
                .find(|&j| {
                    j != i
                        && is_multiple(sorted[i].len(), sorted[j].len())
                        && sorted[j].quality <= Quality::Good
                        && sorted[i].quality >= sorted[j].quality
                })
                .filter(|&j| j > i)
        })
        .collect();
    let mut order = Vec::with_capacity(sorted.len());
    let mut stack: Vec<usize> = (0..sorted.len())
        .rev()
        .filter(|&i| base_of[i].is_none())
        .collect();
    while let Some(i) = stack.pop() {
        order.push(i);
        stack.extend((0..sorted.len()).rev().filter(|&j| base_of[j] == Some(i)));
    }
    let mut slots: Vec<Option<LoopCandidate>> = sorted.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

#[cfg(test)]
mod fixtures {
    //! Signals shared by the loop finder's tests.

    use crate::dsp::rng::SplitMix64;

    /// The sample rate of the test signals.
    pub const RATE: u32 = 44_100;
    /// 0.7 s of frames: the length of the repeating tile.
    pub const TILE: usize = 30_870;
    /// 0.3 s of frames: the fade-in of [`faded_repeat`].
    pub const FADE: usize = 13_230;

    /// `n` samples of seeded noise in -1..1.
    pub fn noise(seed: u64, n: usize) -> Vec<f32> {
        let mut r = SplitMix64::new(seed);
        (0..n).map(|_| r.next_f32() * 2.0 - 1.0).collect()
    }

    /// Four repeats of one tile per channel: every loop of whole tiles is an exact repeat.
    pub fn repeated(channels: &[Vec<f32>]) -> Vec<Vec<f32>> {
        channels
            .iter()
            .map(|t| t.iter().cycle().take(t.len() * 4).copied().collect())
            .collect()
    }

    /// Stereo noise tiles (a different tile per channel), four repeats.
    pub fn stereo_repeat() -> Vec<Vec<f32>> {
        repeated(&[noise(1, TILE), noise(2, TILE)])
    }

    /// 3 s of the stereo tiles repeating, after a 0.3 s fade-in.
    pub fn faded_repeat() -> Vec<Vec<f32>> {
        let tiles = [noise(1, TILE), noise(2, TILE)];
        tiles
            .iter()
            .map(|t| {
                (0..3 * RATE as usize)
                    .map(|i| {
                        let gain = (i as f32 / FADE as f32).min(1.0);
                        t[i % TILE] * gain
                    })
                    .collect()
            })
            .collect()
    }

    /// `audio` played backwards.
    pub fn reversed(audio: &[Vec<f32>]) -> Vec<Vec<f32>> {
        audio
            .iter()
            .map(|ch| ch.iter().rev().copied().collect())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{RATE, TILE, faded_repeat, reversed};
    use super::*;

    fn settings(range: (usize, usize), keep_end: Option<usize>) -> FinderSettings {
        FinderSettings {
            range,
            keep_end,
            min_len: 4_410,
            max_len: None,
            preference: 0.5,
            direction: Direction::Forward,
            max_candidates: MAX_CANDIDATES,
        }
    }

    fn run(audio: &[Vec<f32>], s: &FinderSettings) -> Result<Vec<LoopCandidate>, FinderError> {
        find_loops(audio, RATE, s, &AtomicBool::new(false))
    }

    /// Two loops the finder would call the same.
    fn same_loop(a: &LoopCandidate, b: &LoopCandidate) -> bool {
        let (la, lb) = (a.len() as f64, b.len() as f64);
        let length_close = (la - lb).abs() < 0.01 * la.max(lb);
        let start_close = a.loop_start.abs_diff(b.loop_start) < (0.05 * f64::from(RATE)) as usize;
        length_close
            && (start_close || (a.quality == Quality::Seamless && b.quality == Quality::Seamless))
    }

    #[test]
    fn keep_end_finds_whole_tiles() {
        let a = faded_repeat();
        let len = a[0].len();
        let found = run(&a, &settings((0, len - 1), Some(len - 11))).unwrap();
        let best = &found[0];
        assert_eq!(best.loop_end, len - 11);
        assert_eq!(best.len() % TILE, 0, "{best:?}");
        assert_eq!(best.quality, Quality::Seamless, "{best:?}");
    }

    #[test]
    fn the_shorter_repeat_ranks_first() {
        let a = faded_repeat();
        let len = a[0].len();
        let found = run(&a, &settings((0, len - 1), Some(len - 11))).unwrap();
        let one = found.iter().position(|c| c.len() == TILE);
        let two = found.iter().position(|c| c.len() == 2 * TILE);
        let one = one.expect("a one-tile candidate");
        let two = two.expect("a two-tile candidate");
        assert!(one < two, "{found:?}");
    }

    #[test]
    fn keep_end_at_the_last_frame_works() {
        let a = faded_repeat();
        let len = a[0].len();
        let found = run(&a, &settings((0, len - 1), Some(len - 1))).unwrap();
        assert!(!found.is_empty());
        assert!(found.iter().all(|c| c.loop_end == len - 1));
    }

    #[test]
    fn a_range_from_frame_zero_works() {
        let a = faded_repeat();
        let len = a[0].len();
        let found = run(&a, &settings((0, len - 1), Some(len / 2))).unwrap();
        assert!(!found.is_empty());
    }

    /// The kept ends the shared tests run with: one near the end of the sound, and none.
    fn kept_ends(len: usize) -> [Option<usize>; 2] {
        [Some(len - 11), None]
    }

    #[test]
    fn limits_are_obeyed() {
        let a = faded_repeat();
        let len = a[0].len();
        let range = (1_000, len - 1);
        for keep_end in kept_ends(len) {
            let mut s = settings(range, keep_end);
            s.min_len = 40_000;
            s.max_len = Some(70_000);
            let found = run(&a, &s).unwrap();
            assert!(!found.is_empty(), "{keep_end:?}");
            for c in &found {
                assert!(c.loop_start >= range.0 && c.loop_end <= range.1, "{c:?}");
                assert!((40_000..=70_000).contains(&c.len()), "{c:?}");
            }
        }
    }

    #[test]
    fn no_duplicates() {
        let a = faded_repeat();
        let len = a[0].len();
        for keep_end in kept_ends(len) {
            let found = run(&a, &settings((0, len - 1), keep_end)).unwrap();
            assert!(!found.is_empty(), "{keep_end:?}");
            for (i, x) in found.iter().enumerate() {
                for y in &found[i + 1..] {
                    assert!(!same_loop(x, y), "{x:?} {y:?}");
                }
            }
        }
    }

    #[test]
    fn candidates_are_few_and_finite() {
        let a = faded_repeat();
        let len = a[0].len();
        let found = run(&a, &settings((0, len - 1), Some(len - 11))).unwrap();
        assert!(!found.is_empty() && found.len() <= MAX_CANDIDATES);
        assert!(found.iter().all(|c| c.score.is_finite()));
    }

    #[test]
    fn settings_errors() {
        let a = faded_repeat();
        let len = a[0].len();
        let mut bad = settings((0, len - 1), Some(len - 11));
        bad.max_len = Some(bad.min_len - 1);
        assert_eq!(run(&a, &bad), Err(FinderError::BadSettings));

        let short = settings((1_000, 1_099), Some(1_099));
        assert_eq!(run(&a, &short), Err(FinderError::RangeTooShort));

        let long_audio = vec![vec![0.1f32; 61 * RATE as usize]];
        let long = settings((0, 61 * RATE as usize - 1), None);
        assert_eq!(run(&long_audio, &long), Err(FinderError::RangeTooLong));

        let silence = vec![vec![0.0f32; 3 * RATE as usize]];
        let quiet = settings((0, silence[0].len() - 1), Some(silence[0].len() - 11));
        assert_eq!(run(&silence, &quiet), Err(FinderError::Silent));

        let flag = AtomicBool::new(true);
        let ok = settings((0, len - 1), Some(len - 11));
        assert_eq!(
            find_loops(&a, RATE, &ok, &flag),
            Err(FinderError::Cancelled)
        );
    }

    #[test]
    fn empty_audio_is_too_short() {
        for audio in [Vec::new(), vec![Vec::new(), Vec::new()]] {
            for keep_end in [None, Some(0), Some(1_000)] {
                let s = settings((0, 1_000), keep_end);
                assert_eq!(
                    run(&audio, &s),
                    Err(FinderError::RangeTooShort),
                    "{} channels, {keep_end:?}",
                    audio.len()
                );
            }
        }
    }

    #[test]
    fn backward_keep_end() {
        let a = reversed(&faded_repeat());
        let len = a[0].len();
        let end = 100_000 - 1;
        let mut s = settings((0, len - 1), Some(end));
        s.direction = Direction::Backward;
        let found = run(&a, &s).unwrap();
        let best = &found[0];
        assert!(found.iter().all(|c| c.loop_end == end), "{found:?}");
        assert_eq!(best.len() % TILE, 0, "{best:?}");
        assert_eq!(best.quality, Quality::Seamless, "{best:?}");
    }

    fn cand(start: usize, len: usize, score: f32, quality: Quality) -> LoopCandidate {
        LoopCandidate {
            loop_start: start,
            loop_end: start + len - 1,
            score,
            quality,
            // Only the tone is off, so the seam score is the score given.
            measures: SeamMeasures {
                click: 0.0,
                tone: score,
                matching: 1.0,
            },
            period: None,
        }
    }

    fn lens(found: &[LoopCandidate]) -> Vec<usize> {
        found.iter().map(LoopCandidate::len).collect()
    }

    #[test]
    fn a_multiple_follows_its_base_unless_it_is_better() {
        let (t, x) = (10_000, 7_000);
        let worse = vec![
            cand(0, 2 * t, 0.5, Quality::Good),
            cand(0, t, 0.1, Quality::Good),
            cand(5_000, x, 0.3, Quality::Fair),
            cand(0, 3 * t, 0.6, Quality::Good),
        ];
        // The doubled loop and the tripled loop are no better than the single, so both follow it.
        let ranked = rank(worse, 0.5, 100_000, 8, RATE);
        assert_eq!(lens(&ranked), [t, 2 * t, 3 * t, x]);
        // A strictly better word keeps the doubled loop ahead.
        let better = vec![
            cand(0, 2 * t, 0.05, Quality::Seamless),
            cand(0, t, 0.1, Quality::Good),
            cand(5_000, x, 0.3, Quality::Fair),
        ];
        let ranked = rank(better, 0.5, 100_000, 8, RATE);
        assert_eq!(lens(&ranked), [2 * t, t, x]);
        // The same word with a slightly lower seam score is no better: the doubled loop ranks
        // directly below the single, neutral or with the preference crediting the longer loop.
        for preference in [0.5, 1.0] {
            let same_word = vec![
                cand(0, 2 * t, 0.07, Quality::Good),
                cand(0, t, 0.10, Quality::Good),
                cand(5_000, x, 0.3, Quality::Fair),
            ];
            let ranked = rank(same_word, preference, 100_000, 8, RATE);
            assert_eq!(lens(&ranked), [t, 2 * t, x], "preference {preference}");
            assert!(ranked[1].score < ranked[0].score);
        }
        // The preference credits the longer loop, putting its ranking score ahead of the
        // single's, but its seam is still no better, so it ranks directly below the single.
        let long_seam = vec![
            cand(0, 2 * t, 0.11, Quality::Good),
            cand(0, t, 0.10, Quality::Good),
            cand(5_000, x, 0.3, Quality::Fair),
        ];
        let ranked = rank(long_seam, 1.0, 100_000, 8, RATE);
        assert_eq!(lens(&ranked), [t, 2 * t, x]);
        assert!(ranked[1].score < ranked[0].score);
        // A Fair or Rough base is not a genuine repeat, so it pulls nothing below it: the
        // better-scored multiple keeps the place the plain sort gives it.
        for word in [Quality::Fair, Quality::Rough] {
            let weak_base = vec![
                cand(0, 2 * t, 4.0, word),
                cand(5_000, x, 5.0, word),
                cand(0, t, 6.0, word),
            ];
            let ranked = rank(weak_base, 0.5, 100_000, 8, RATE);
            assert_eq!(lens(&ranked), [2 * t, x, t], "{word:?}");
        }
    }

    #[test]
    fn a_multiple_already_below_its_base_stays_put() {
        let (t, x) = (10_000, 7_000);
        // The doubled loop is Good like its base but scores worse than x, so it stays below x.
        let found = vec![
            cand(0, t, 0.1, Quality::Good),
            cand(5_000, x, 0.2, Quality::Good),
            cand(0, 2 * t, 0.3, Quality::Good),
        ];
        assert_eq!(lens(&rank(found, 0.5, 100_000, 8, RATE)), [t, x, 2 * t]);
    }

    #[test]
    fn a_fair_candidate_never_ranks_above_a_good_one() {
        // However much better its score, even with the preference crediting its length.
        for preference in [0.0, 0.5, 1.0] {
            let found = vec![
                cand(0, 60_000, 0.01, Quality::Fair),
                cand(20_000, 7_000, 0.9, Quality::Good),
                cand(40_000, 9_000, 0.02, Quality::Rough),
                cand(50_000, 8_000, 0.5, Quality::Seamless),
            ];
            let ranked = rank(found, preference, 100_000, 8, RATE);
            let words: Vec<Quality> = ranked.iter().map(|c| c.quality).collect();
            assert_eq!(
                words,
                [
                    Quality::Seamless,
                    Quality::Good,
                    Quality::Fair,
                    Quality::Rough
                ],
                "preference {preference}"
            );
        }
    }

    #[test]
    fn a_rough_multiple_does_not_jump_above_good_candidates() {
        let (t, y, z) = (10_000, 7_000, 13_000);
        let found = vec![
            cand(0, t, 0.3, Quality::Good),
            cand(20_000, y, 0.4, Quality::Good),
            cand(40_000, z, 0.1, Quality::Fair),
            // Twice the first loop's length, with the best score of all.
            cand(60_000, 2 * t, 0.05, Quality::Rough),
        ];
        assert_eq!(lens(&rank(found, 0.5, 100_000, 8, RATE)), [t, y, z, 2 * t]);
    }

    #[test]
    fn the_length_credit_never_exceeds_its_cap() {
        // A kept end past the range can give a loop longer than the range.
        let ranked = rank(
            vec![cand(0, 30_000, 0.5, Quality::Good)],
            1.0,
            10_000,
            8,
            RATE,
        );
        assert!((ranked[0].score - (0.5 - LENGTH_CREDIT)).abs() < 1e-6);
    }

    #[test]
    fn the_preference_favours_short_or_long_loops() {
        let make = || {
            vec![
                cand(0, 10_000, 0.20, Quality::Good),
                cand(20_000, 45_000, 0.25, Quality::Good),
            ]
        };
        assert_eq!(lens(&rank(make(), 0.5, 100_000, 8, RATE)), [10_000, 45_000]);
        assert_eq!(lens(&rank(make(), 1.0, 100_000, 8, RATE)), [45_000, 10_000]);
        assert_eq!(lens(&rank(make(), 0.0, 100_000, 8, RATE)), [10_000, 45_000]);
        let ranked = rank(make(), 1.0, 100_000, 8, RATE);
        assert!((ranked[0].score - (0.25 - 0.15 * 0.45)).abs() < 1e-6);
    }

    #[test]
    fn duplicates_and_the_limit() {
        let found = vec![
            cand(1_000, 20_000, 0.2, Quality::Good),
            // Nearly the same loop, a little worse.
            cand(1_500, 20_100, 0.3, Quality::Good),
            // Far enough away.
            cand(40_000, 20_000, 0.4, Quality::Good),
            cand(60_000, 9_000, 0.0, Quality::Seamless),
            // Seamless but 5 % longer: not the same loop.
            cand(70_000, 9_500, 0.001, Quality::Seamless),
            // Seamless, far away, within 1 % of the first: the same loop.
            cand(80_000, 9_050, 0.002, Quality::Seamless),
        ];
        let ranked = rank(found.clone(), 0.5, 100_000, 8, RATE);
        let starts: Vec<usize> = ranked.iter().map(|c| c.loop_start).collect();
        assert_eq!(starts, [60_000, 70_000, 1_000, 40_000]);
        assert_eq!(rank(found, 0.5, 100_000, 2, RATE).len(), 2);
    }

    #[test]
    fn ties_go_to_the_earlier_start_then_the_shorter_loop() {
        let found = vec![
            cand(9_000, 6_000, 0.2, Quality::Fair),
            cand(1_000, 25_000, 0.2, Quality::Fair),
            cand(1_000, 8_000, 0.2, Quality::Fair),
        ];
        let ranked = rank(found, 0.5, 100_000, 8, RATE);
        assert_eq!(lens(&ranked), [8_000, 25_000, 6_000]);
    }

    #[test]
    fn a_far_kept_end_searches_only_the_last_minute() {
        let rate = 8_000;
        let n = 100 * rate as usize;
        let audio = vec![super::fixtures::noise(7, n)];
        let end = n - 11;
        let mut s = settings((0, 50 * rate as usize), Some(end));
        s.min_len = 800;
        let found = find_loops(&audio, rate, &s, &AtomicBool::new(false)).unwrap();
        assert!(!found.is_empty());
        let reach = 60 * rate as usize;
        for c in &found {
            assert_eq!(c.loop_end, end);
            assert!(c.loop_start + reach > end, "{c:?}");
            assert!(c.len() <= reach, "{c:?}");
        }
    }

    #[test]
    fn deterministic() {
        let a = faded_repeat();
        let len = a[0].len();
        for keep_end in kept_ends(len) {
            let s = settings((0, len - 1), keep_end);
            assert_eq!(run(&a, &s), run(&a, &s));
        }
    }

    /// The steady part of `audio`, as the panel would search it.
    fn steady(audio: &[Vec<f32>]) -> (usize, usize) {
        let len = audio[0].len();
        let r = steady::search_range(audio, RATE, None, (0, len - 1)).unwrap();
        (r.first, r.last)
    }

    /// The largest difference between the 256 frames from the loop start and the 256 after the
    /// loop end, over channels.
    fn splice_error(audio: &[Vec<f32>], c: &LoopCandidate) -> f32 {
        let (s, e) = (c.loop_start, c.loop_end);
        audio
            .iter()
            .flat_map(|ch| (0..256).map(move |k| (ch[s + k] - ch[e + 1 + k]).abs()))
            .fold(0.0, f32::max)
    }

    #[test]
    fn whole_range_finds_the_tile() {
        let a = faded_repeat();
        let found = run(&a, &settings(steady(&a), None)).unwrap();
        let best = &found[0];
        assert_eq!(best.len() % TILE, 0, "{best:?}");
        assert!(splice_error(&a, best) < 1e-3, "{best:?}");
        assert_eq!(best.quality, Quality::Seamless, "{best:?}");
    }

    /// 3 s of a 220 Hz tone with 8 equal harmonics and a 2 Hz vibrato of ±5 cents.
    fn vibrato_tone() -> Vec<Vec<f32>> {
        let rate = f64::from(RATE);
        let mut phase = 0.0f64;
        let tone = (0..3 * RATE as usize)
            .map(|i| {
                let t = i as f64 / rate;
                let cents = 5.0 * (std::f64::consts::TAU * 2.0 * t).sin();
                let v = (1..=8).map(|k| (k as f64 * phase).sin()).sum::<f64>() / 8.0;
                phase += std::f64::consts::TAU * 220.0 * 2f64.powf(cents / 1_200.0) / rate;
                v as f32
            })
            .collect();
        vec![tone]
    }

    #[test]
    fn a_tone_with_vibrato_loops_cleanly() {
        let a = vibrato_tone();
        let found = run(&a, &settings(steady(&a), None)).unwrap();
        let best = &found[0];
        assert!(best.measures.click <= 1.0, "{best:?}");
        let c = best.cycles().expect("a tonal range");
        assert!((c - c.round()).abs() <= 0.1, "{c} {best:?}");
    }

    #[test]
    fn white_noise_is_at_best_fair() {
        let a = vec![super::fixtures::noise(11, 3 * RATE as usize)];
        let len = a[0].len();
        let found = run(&a, &settings((0, len - 1), None)).unwrap();
        assert!(!found.is_empty());
        for c in &found {
            assert!(c.quality >= Quality::Fair, "{c:?}");
        }
    }

    #[test]
    fn backward_whole_range() {
        let a = reversed(&faded_repeat());
        let mut s = settings(steady(&a), None);
        s.direction = Direction::Backward;
        let found = run(&a, &s).unwrap();
        let best = &found[0];
        assert_eq!(best.len() % TILE, 0, "{best:?}");
        assert_eq!(best.quality, Quality::Seamless, "{best:?}");
    }

    /// How long a whole-range search of `seconds` of stereo noise takes. Run with
    /// `cargo test --release -p limewave-core -- --ignored --nocapture whole_range_speed`.
    #[test]
    #[ignore = "a timing measurement, for release builds"]
    fn whole_range_speed() {
        for seconds in [12, 60] {
            let n = seconds * RATE as usize;
            let a = vec![super::fixtures::noise(3, n), super::fixtures::noise(4, n)];
            let s = settings((0, n - 1), None);
            let begun = std::time::Instant::now();
            let found = run(&a, &s).unwrap();
            eprintln!(
                "{seconds} s: {:?}, {} candidates",
                begun.elapsed(),
                found.len()
            );
        }
    }
}
