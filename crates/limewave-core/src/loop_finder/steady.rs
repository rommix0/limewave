//! The part of a sound the loop finder searches: the user's selection, or else the steady
//! part found from the level envelope.

use super::{FinderError, MAX_SEARCH_SECONDS};
use crate::dsp::envelope::{SILENCE_DB, level_db, rms_db_frames};

/// Length of the level steps, in seconds.
const STEP_S: f64 = 0.050;

/// Width of the running median that smooths the levels, in seconds. Rounded to a whole
/// number of steps either side of the centre, so the window is centred.
const SMOOTH_S: f64 = 0.5;

/// How far a smoothed level may stray from the reference and still count as steady, in dB.
/// It is also how far the level may fall before the sound counts as over.
const BAND_DB: f32 = 3.0;

/// A level below this counts as silence, in dB. Not the same as `SILENCE_DB`, the level
/// reported for exact digital zero.
const AUDIBLE_FLOOR_DB: f32 = -120.0;

/// The shortest steady part, in seconds...
const MIN_STEADY_S: f64 = 0.25;

/// ...or this fraction of the sound, whichever is longer.
const MIN_STEADY_FRACTION: f64 = 0.2;

/// How a [`SearchRange`] was chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RangeKind {
    /// The user's selection.
    Selection,
    /// The steady part, found from the level envelope.
    Steady,
    /// No steady part was found, so this is the middle 60 % of the sound.
    NoSteadyPart,
}

/// A range of frames to search for loops, both ends inclusive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SearchRange {
    /// The first frame.
    pub first: usize,
    /// The last frame.
    pub last: usize,
    /// How the range was chosen.
    pub kind: RangeKind,
}

/// The centred running median of `levels`, `half` steps either side (the window shrinks at
/// the ends).
fn running_median(levels: &[f32], half: usize) -> Vec<f32> {
    let mut window = Vec::with_capacity(2 * half + 1);
    (0..levels.len())
        .map(|i| {
            window.clear();
            window.extend_from_slice(
                &levels[i.saturating_sub(half)..(i + half + 1).min(levels.len())],
            );
            window.sort_by(f32::total_cmp);
            window[window.len() / 2]
        })
        .collect()
}

/// The middle 60 % of the first `len` (non-zero) frames, the fallback when nothing is steady.
/// It never reaches past `audible_end`, so a quiet tail cannot push it into silence. If that
/// leaves less than one `hop`, the whole audible span is used instead.
fn middle_range(len: usize, audible_end: usize, hop: usize) -> SearchRange {
    let last = (len * 4 / 5).min(len - 1).min(audible_end - 1);
    let first = (len / 5).min(last);
    let (first, last) = if last - first + 1 < hop {
        (0, audible_end - 1)
    } else {
        (first, last)
    };
    SearchRange {
        first,
        last,
        kind: RangeKind::NoSteadyPart,
    }
}

/// Whether every sample of `first..=last` is quieter than −120 dB: no whole 50 ms step and no
/// leftover tail of the range reaches that level.
pub(super) fn range_is_silent(audio: &[Vec<f32>], rate: u32, first: usize, last: usize) -> bool {
    let hop = ((f64::from(rate) * STEP_S) as usize).max(1);
    let levels = rms_db_frames(audio, first, last, hop);
    let tail = first + levels.len() * hop;
    levels.iter().all(|&d| d < AUDIBLE_FLOOR_DB)
        && (tail > last || level_db(audio, tail, last) < AUDIBLE_FLOOR_DB)
}

/// The steady part of the sound: where the level has settled after the attack and before the
/// fade.
///
/// - Levels are RMS in 50 ms steps, energy summed over channels.
/// - The reference is the median level of the second half. The levels are smoothed with a
///   centred running median 0.5 s wide.
/// - The sound ends at the last step whose smoothed level is within 3 dB of the reference or
///   above. The steady part starts at the first step from which every smoothed level up to
///   that end is within 3 dB of the reference.
/// - It counts as found when it is at least 0.25 s and 20 % of the sound long. Otherwise the
///   result is the middle 60 % of the sound, with kind [`RangeKind::NoSteadyPart`].
///
/// `None` for empty audio or silence (every step quieter than −120 dB).
pub fn steady_range(audio: &[Vec<f32>], rate: u32) -> Option<SearchRange> {
    let len = audio.iter().map(Vec::len).min().filter(|&n| n > 0)?;
    let hop = ((f64::from(rate) * STEP_S) as usize).max(1);
    let levels = rms_db_frames(audio, 0, len - 1, hop);
    if levels.is_empty() {
        // Shorter than one step: nothing to measure, so only silence can be ruled out.
        return (level_db(audio, 0, len - 1) >= AUDIBLE_FLOOR_DB)
            .then(|| middle_range(len, len, hop));
    }
    // Trailing silence is not part of the sound: it would drag the reference down and read as
    // steady itself. No audible step at all is silence.
    let audible = levels.iter().rposition(|&d| d >= AUDIBLE_FLOOR_DB)? + 1;
    // The fallback is based on the sound up to its last non-zero step, so a long decay below
    // -120 dB still counts as sound, but exact silence after it does not.
    let sounding = levels
        .iter()
        .rposition(|&d| d > SILENCE_DB)
        .map_or(audible, |i| i + 1);
    let sounding_len = if sounding == levels.len() {
        len
    } else {
        sounding * hop
    };
    let levels = &levels[..audible];

    let mut second_half = levels[levels.len() / 2..].to_vec();
    second_half.sort_by(f32::total_cmp);
    let reference = second_half[second_half.len() / 2];

    let half = ((SMOOTH_S / STEP_S / 2.0).round() as usize).max(1);
    let smooth = running_median(levels, half);
    let steady = smooth
        .iter()
        .rposition(|&d| d >= reference - BAND_DB)
        .and_then(|end| {
            let start = smooth[..=end]
                .iter()
                .rposition(|&d| (d - reference).abs() > BAND_DB)
                .map_or(0, |i| i + 1);
            (start <= end).then_some((start, end))
        });
    let found = steady.filter(|&(start, end)| {
        let frames = ((end + 1 - start) * hop) as f64;
        frames >= (MIN_STEADY_S * f64::from(rate)).max(MIN_STEADY_FRACTION * (audible * hop) as f64)
    });
    Some(match found {
        Some((start, end)) => SearchRange {
            first: start * hop,
            last: (end + 1) * hop - 1,
            kind: RangeKind::Steady,
        },
        None => middle_range(sounding_len, audible * hop, hop),
    })
}

/// The range the loop finder searches, within the `trim` (Start..End, inclusive).
///
/// - With a `selection` (the ends in either order), that selection. One longer than 60 s is
///   [`FinderError::RangeTooLong`].
/// - Without one, the [`steady_range`], keeping only its last 60 s if it is longer. Silence is
///   [`FinderError::Silent`].
/// - Either is cut to the trim; nothing left is [`FinderError::RangeTooShort`].
/// - A range that is silent after that is [`FinderError::Silent`].
pub fn search_range(
    audio: &[Vec<f32>],
    rate: u32,
    selection: Option<(usize, usize)>,
    trim: (usize, usize),
) -> Result<SearchRange, FinderError> {
    let max_frames = (MAX_SEARCH_SECONDS * f64::from(rate)) as usize;
    let (range, kind) = match selection {
        Some((a, b)) => ((a.min(b), a.max(b)), RangeKind::Selection),
        None => {
            let steady = steady_range(audio, rate).ok_or(FinderError::Silent)?;
            ((steady.first, steady.last), steady.kind)
        }
    };
    let first = range.0.max(trim.0);
    let last = range.1.min(trim.1);
    if first > last {
        return Err(FinderError::RangeTooShort);
    }
    let first = if last + 1 - first <= max_frames {
        first
    } else if kind == RangeKind::Selection {
        return Err(FinderError::RangeTooLong);
    } else {
        last + 1 - max_frames
    };
    if range_is_silent(audio, rate, first, last) {
        return Err(FinderError::Silent);
    }
    Ok(SearchRange { first, last, kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rng::SplitMix64;

    const RATE: u32 = 44_100;

    /// `secs` of noise at amplitude 0.3 whose level moves linearly in dB from `from_db` to
    /// `to_db`.
    fn db_ramp(rng: &mut SplitMix64, secs: f64, from_db: f32, to_db: f32) -> Vec<f32> {
        let n = (secs * f64::from(RATE)) as usize;
        (0..n)
            .map(|i| {
                let db = from_db + (to_db - from_db) * i as f32 / n as f32;
                0.3 * 10f32.powf(db / 20.0) * (rng.next_f32() * 2.0 - 1.0)
            })
            .collect()
    }

    #[test]
    fn steady_part_skips_attack_and_fade() {
        let mut rng = SplitMix64::new(1);
        let mut x = db_ramp(&mut rng, 0.5, -60.0, 0.0);
        x.extend(db_ramp(&mut rng, 3.0, 0.0, 0.0));
        x.extend(db_ramp(&mut rng, 1.0, 0.0, -60.0));
        let r = steady_range(&[x], RATE).expect("sound");
        assert_eq!(r.kind, RangeKind::Steady);
        assert!(r.first.abs_diff(22_050) <= 4_410, "first {}", r.first);
        assert!(r.last.abs_diff(154_350) <= 4_410, "last {}", r.last);
    }

    #[test]
    fn a_pure_decay_has_no_steady_part() {
        let mut rng = SplitMix64::new(2);
        let x = db_ramp(&mut rng, 2.0, 0.0, -120.0);
        let len = x.len();
        let r = steady_range(&[x], RATE).expect("sound");
        assert_eq!(r.kind, RangeKind::NoSteadyPart);
        assert!(r.first.abs_diff(len / 5) <= 1);
        assert!(r.last.abs_diff(len * 4 / 5) <= 1);
    }

    #[test]
    fn silence_and_empty_give_none() {
        assert_eq!(steady_range(&[], RATE), None);
        assert_eq!(steady_range(&[vec![]], RATE), None);
        assert_eq!(steady_range(&[vec![0.0; 44_100]], RATE), None);
    }

    #[test]
    fn trailing_silence_is_not_the_steady_part() {
        let mut rng = SplitMix64::new(4);
        let mut x = db_ramp(&mut rng, 1.0, 0.0, 0.0);
        x.extend(vec![0.0; 2 * 44_100]);
        let r = steady_range(&[x], RATE).expect("sound");
        assert_eq!(r.kind, RangeKind::Steady);
        assert!(r.last < 44_100 + 2_205, "last {}", r.last);
    }

    #[test]
    fn a_decay_followed_by_silence_still_has_a_range() {
        let mut rng = SplitMix64::new(6);
        let mut x = db_ramp(&mut rng, 2.0, 0.0, -120.0);
        x.extend(vec![0.0; 8 * 44_100]);
        let len = x.len();
        let audio = [x];
        let r = steady_range(&audio, RATE).expect("sound");
        assert_eq!(r.kind, RangeKind::NoSteadyPart);
        assert!(r.first <= r.last && r.last < 88_200, "{r:?}");
        let r = search_range(&audio, RATE, None, (0, len - 1)).expect("audible");
        assert_eq!(r.kind, RangeKind::NoSteadyPart);
    }

    #[test]
    fn a_short_audible_span_is_used_whole() {
        let mut rng = SplitMix64::new(7);
        // 0.1 s at 0 dB, then 1 s at -150 dB (below the audible floor), then exact zeros.
        let mut x = db_ramp(&mut rng, 0.1, 0.0, 0.0);
        x.extend(db_ramp(&mut rng, 1.0, -150.0, -150.0));
        x.extend(vec![0.0; 44_100]);
        let r = steady_range(&[x], RATE).expect("sound");
        assert_eq!(r.kind, RangeKind::NoSteadyPart);
        assert_eq!(r.first, 0);
        assert!(r.last.abs_diff(4_409) <= 2_205, "{r:?}");
        assert!(r.last + 1 - r.first >= 2_205, "{r:?}");
    }

    #[test]
    fn a_silent_selection_is_refused() {
        let mut rng = SplitMix64::new(5);
        let mut x = db_ramp(&mut rng, 1.0, 0.0, 0.0);
        x.extend(vec![0.0; 44_100]);
        let audio = [x];
        let trim = (0, 88_199);
        assert_eq!(
            search_range(&audio, RATE, Some((50_000, 80_000)), trim),
            Err(FinderError::Silent)
        );
        // Exactly one step, and silent.
        assert_eq!(
            search_range(&audio, RATE, Some((50_000, 52_204)), trim),
            Err(FinderError::Silent)
        );
        // Shorter than one step, and silent.
        assert_eq!(
            search_range(&audio, RATE, Some((50_000, 50_100)), trim),
            Err(FinderError::Silent)
        );
        // Part audible is fine.
        assert!(search_range(&audio, RATE, Some((40_000, 80_000)), trim).is_ok());
        // So is a short range with sound in it.
        assert!(search_range(&audio, RATE, Some((100, 200)), trim).is_ok());
    }

    #[test]
    fn search_range_rules() {
        let mut rng = SplitMix64::new(3);
        let audio = [db_ramp(&mut rng, 2.0, 0.0, 0.0)];
        let last = audio[0].len() - 1;

        let r = search_range(&audio, RATE, Some((5000, 1000)), (0, last)).unwrap();
        assert_eq!(
            (r.first, r.last, r.kind),
            (1000, 5000, RangeKind::Selection)
        );

        let r = search_range(&audio, RATE, Some((0, 9999)), (2000, 8000)).unwrap();
        assert_eq!((r.first, r.last), (2000, 8000));

        // Outside the trim, nothing is left.
        assert_eq!(
            search_range(&audio, RATE, Some((0, 1000)), (2000, 8000)),
            Err(FinderError::RangeTooShort)
        );

        // A selection over 60 s is refused.
        let long = 61 * RATE as usize;
        assert_eq!(
            search_range(&audio, RATE, Some((0, long)), (0, long)),
            Err(FinderError::RangeTooLong)
        );

        // A 70 s steady sound keeps the last 60 s of its steady part.
        let big = [db_ramp(&mut rng, 70.0, 0.0, 0.0)];
        let steady = steady_range(&big, RATE).unwrap();
        let r = search_range(&big, RATE, None, (0, big[0].len() - 1)).unwrap();
        assert_eq!(r.kind, RangeKind::Steady);
        assert_eq!(r.last, steady.last);
        assert_eq!(r.last + 1 - r.first, 60 * 44_100);

        let silent = [vec![0.0f32; 44_100]];
        assert_eq!(
            search_range(&silent, RATE, None, (0, 44_099)),
            Err(FinderError::Silent)
        );
    }
}
