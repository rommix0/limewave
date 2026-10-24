//! Seam measures: how well a candidate loop joins, judged against the sound's own change.
//!
//! A forward loop plays `x[s..=e]` and then jumps from `x[e]` back to `x[s]`. Three measures
//! say how audible that jump is. Each is relative to the material itself, so a loop in a busy
//! sound is not punished for changes the sound makes anyway.

use std::ops::Range;

use crate::dsp::spectrum::{Fft, hann};

/// The smallest second-difference energy [`natural_change`] reports, so dividing by it is safe.
const D2_FLOOR: f32 = 1e-20;

/// The smallest band-level change [`natural_change`] reports, in dB. A difference this small
/// between band levels is treated as inaudible, so a steady pure tone, whose own change is
/// nearly zero, is not ranked Rough for differences nobody could hear.
pub const FLUX_FLOOR: f32 = 0.5;

/// How far below the loudest band, in dB, a band level is allowed to count when two frames are
/// compared. Quieter bands are raised to this, so they cannot dominate the tone measure.
pub const TONE_RANGE_DB: f32 = 60.0;

/// The lowest band level, in dB. Silent bands sit here.
const LEVEL_FLOOR_DB: f32 = -120.0;

/// The lower edge of the lowest band, in Hz. Bands are a third of an octave wide.
const LOWEST_BAND_HZ: f64 = 50.0;

/// The tone frame length before rounding to a power of two, in seconds.
const TONE_FRAME_S: f64 = 0.046;

/// The shortest tone frame, in samples.
const MIN_TONE_FRAME: usize = 64;

/// Half the span of the matching context, in seconds.
const MATCH_CONTEXT_S: f64 = 0.005;

/// How good a loop point pair sounds, best first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Quality {
    /// The join is inaudible.
    Seamless,
    /// The join is hard to hear.
    Good,
    /// The join can be heard but is acceptable.
    Fair,
    /// The join is plainly audible.
    Rough,
}

/// How a loop's join compares with the sound's own change. Lower `click` and `tone` and higher
/// `matching` are better.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SeamMeasures {
    /// The jump in the second difference at the join, relative to the sound's own
    /// ([`Natural::d2`]). 0 means the join changes nothing.
    pub click: f32,
    /// The change in third-octave band levels across the join, relative to the sound's own
    /// ([`Natural::flux`]). 0 means identical.
    pub tone: f32,
    /// The normalised correlation of the audio either side of the join, -1 to 1. 1 is an exact
    /// repeat; 0 is unrelated, or silent.
    pub matching: f32,
}

/// The sound's own change over a range, the yardstick for [`SeamMeasures`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Natural {
    /// The median second-difference energy per frame, summed over channels.
    pub d2: f32,
    /// The median RMS change in band level between consecutive tone frames, in dB.
    pub flux: f32,
}

/// The limits for one [`Quality`] row: a pair of loop points reaches the row when its click and
/// tone are at most these and its matching is at least this.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Threshold {
    /// The largest [`SeamMeasures::click`].
    pub click: f32,
    /// The largest [`SeamMeasures::tone`].
    pub tone: f32,
    /// The smallest [`SeamMeasures::matching`].
    pub matching: f32,
}

/// The limits for [`Quality::Seamless`].
pub const SEAMLESS: Threshold = Threshold {
    click: 0.1,
    tone: 0.25,
    matching: 0.995,
};

/// The limits for [`Quality::Good`].
pub const GOOD: Threshold = Threshold {
    click: 1.0,
    tone: 0.75,
    matching: 0.95,
};

/// The limits for [`Quality::Fair`]. Anything outside them is [`Quality::Rough`].
pub const FAIR: Threshold = Threshold {
    click: 4.0,
    tone: 1.5,
    matching: 0.7,
};

/// The length of a tone frame at `rate`, in samples: 46 ms rounded to a power of two (2,048 at
/// 44,100 Hz).
pub fn tone_frame(rate: u32) -> usize {
    let target = (TONE_FRAME_S * f64::from(rate)).max(1.0);
    (1usize << (target.log2().round() as u32)).max(MIN_TONE_FRAME)
}

/// The sound's own change over `first..=last`, clamped to the audio, summed over channels.
///
/// - `d2` is the median second-difference energy per frame.
/// - `flux` is the median RMS-dB band difference between consecutive, non-overlapping tone
///   frames. With fewer than two frames in the range it is at its floor.
///
/// `d2` is floored at a tiny positive value and `flux` at [`FLUX_FLOOR`] (0.5 dB), so both are
/// safe to divide by.
pub fn natural_change(audio: &[Vec<f32>], first: usize, last: usize, rate: u32) -> Natural {
    let len = shortest(audio);
    let last = last.min(len.saturating_sub(1));
    let mut d2 = Vec::new();
    if len >= 3 {
        for i in first.max(1)..=last.min(len - 2) {
            let sum: f64 = audio
                .iter()
                .map(|ch| second_difference(ch, i).powi(2))
                .sum();
            d2.push(sum as f32);
        }
    }
    let n = tone_frame(rate);
    let bands = Bands::new(n, rate);
    let mut levels = Vec::new();
    let mut start = first;
    while start + n <= last + 1 {
        levels.push(bands.levels(audio, start..start + n, false));
        start += n;
    }
    let mut flux: Vec<f32> = levels.windows(2).map(|w| rms_diff(&w[0], &w[1])).collect();
    Natural {
        d2: median(&mut d2).unwrap_or(0.0).max(D2_FLOOR),
        flux: median(&mut flux).unwrap_or(0.0).max(FLUX_FLOOR),
    }
}

/// The seam of the forward loop `loop_start..=loop_end`, measured against `natural`.
///
/// - `click` compares the two second differences that straddle the jump `x[e] → x[s]` with
///   those of the true continuation (`x[e+1]`, `x[e+2]`), summed over channels, over
///   `natural.d2`. When the loop ends within two frames of the end of the audio there is no
///   continuation, so the start's own lead-in (`x[s-2]`, `x[s-1]`) stands in.
/// - `tone` is the mean RMS-dB difference between the tone frames ending at `e + 1` and at `s`,
///   and, where both fit, those starting there, over `natural.flux`.
/// - `matching` is the normalised correlation of the 5 ms either side of `e + 1` and of `s`,
///   both clipped to the same lengths at the edges of the audio.
///
/// Loop points that do not make a loop (`e <= s`, or past the audio) measure as the worst
/// possible seam: infinite `click` and `tone` and `matching` 0, so the quality is
/// [`Quality::Rough`] and the [`score`] is infinite, never better than a real candidate's.
pub fn seam_measures(
    audio: &[Vec<f32>],
    loop_start: usize,
    loop_end: usize,
    rate: u32,
    natural: &Natural,
) -> SeamMeasures {
    let len = shortest(audio);
    let (s, e) = (loop_start, loop_end);
    if e <= s || e >= len {
        return SeamMeasures {
            click: f32::INFINITY,
            tone: f32::INFINITY,
            matching: 0.0,
        };
    }
    SeamMeasures {
        click: click(audio, s, e, len, natural.d2),
        tone: tone(audio, s, e, len, rate, natural.flux),
        matching: matching(audio, s, e, len, rate),
    }
}

/// The word for a seam: the first row of [`SEAMLESS`], [`GOOD`] and [`FAIR`] that all three
/// measures meet, else [`Quality::Rough`].
pub fn quality(m: &SeamMeasures) -> Quality {
    let meets = |t: &Threshold| m.click <= t.click && m.tone <= t.tone && m.matching >= t.matching;
    if meets(&SEAMLESS) {
        Quality::Seamless
    } else if meets(&GOOD) {
        Quality::Good
    } else if meets(&FAIR) {
        Quality::Fair
    } else {
        Quality::Rough
    }
}

/// One number for ranking seams: `ln(1 + click) + tone + 4·(1 − matching)`. Lower is better.
pub fn score(m: &SeamMeasures) -> f32 {
    m.click.ln_1p() + m.tone + 4.0 * (1.0 - m.matching)
}

/// The length of the shortest channel, 0 for no channels.
pub(super) fn shortest(audio: &[Vec<f32>]) -> usize {
    audio.iter().map(Vec::len).min().unwrap_or(0)
}

/// `ch[i-1] − 2·ch[i] + ch[i+1]`. `i` must have a neighbour either side.
fn second_difference(ch: &[f32], i: usize) -> f64 {
    f64::from(ch[i - 1]) - 2.0 * f64::from(ch[i]) + f64::from(ch[i + 1])
}

/// The median of `v` (the mean of the middle pair when even), reordering it. `None` if empty.
fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    let odd = v.len() % 2 == 1;
    let (lower, upper, _) = v.select_nth_unstable_by(v.len() / 2, f32::total_cmp);
    let upper = *upper;
    if odd {
        return Some(upper);
    }
    let below = lower.iter().copied().fold(f32::MIN, f32::max);
    Some((below + upper) / 2.0)
}

/// The jump in second difference at the join, over `d2`.
fn click(audio: &[Vec<f32>], s: usize, e: usize, len: usize, d2: f32) -> f32 {
    let mut sum = 0.0f64;
    for ch in audio {
        let x = |i: usize| f64::from(ch[i]);
        // The two second differences that straddle x[e] -> x[s].
        let spliced = (x(e - 1) - 2.0 * x(e) + x(s), x(e) - 2.0 * x(s) + x(s + 1));
        let truth = if e + 2 < len {
            (
                x(e - 1) - 2.0 * x(e) + x(e + 1),
                x(e) - 2.0 * x(e + 1) + x(e + 2),
            )
        } else {
            // No continuation: what leads into the start stands in for it.
            let (p2, p1) = (x(s.saturating_sub(2)), x(s.saturating_sub(1)));
            (p2 - 2.0 * p1 + x(s), p1 - 2.0 * x(s) + x(s + 1))
        };
        sum += (spliced.0 - truth.0).powi(2) + (spliced.1 - truth.1).powi(2);
    }
    (sum / f64::from(d2.max(D2_FLOOR))) as f32
}

/// The mean band-level difference across the join, over `flux`.
fn tone(audio: &[Vec<f32>], s: usize, e: usize, len: usize, rate: u32, flux: f32) -> f32 {
    let n = tone_frame(rate);
    let bands = Bands::new(n, rate);
    let mut diffs = Vec::new();
    // The frames ending at e + 1 and at s; near the start of the audio, as much as there is.
    let m = n.min(s);
    if m > 0 {
        let end = bands.levels(audio, e + 1 - m..e + 1, true);
        let start = bands.levels(audio, s - m..s, true);
        diffs.push(rms_diff(&end, &start));
    }
    // The frames starting there, when both fit.
    if e + 1 + n <= len {
        let after_end = bands.levels(audio, e + 1..e + 1 + n, false);
        let after_start = bands.levels(audio, s..s + n, false);
        diffs.push(rms_diff(&after_end, &after_start));
    }
    if diffs.is_empty() {
        return 0.0;
    }
    let mean = diffs.iter().sum::<f32>() / diffs.len() as f32;
    mean / flux.max(FLUX_FLOOR)
}

/// The normalised correlation of the audio around `e + 1` and around `s`.
fn matching(audio: &[Vec<f32>], s: usize, e: usize, len: usize, rate: u32) -> f32 {
    let context = (MATCH_CONTEXT_S * f64::from(rate)).round() as usize;
    let before = context.min(s);
    let after = context.min(len - (e + 1));
    let (mut dot, mut energy_a, mut energy_b) = (0.0f64, 0.0f64, 0.0f64);
    for ch in audio {
        let a = &ch[e + 1 - before..e + 1 + after];
        let b = &ch[s - before..s + after];
        for (&p, &q) in a.iter().zip(b) {
            let (p, q) = (f64::from(p), f64::from(q));
            dot += p * q;
            energy_a += p * p;
            energy_b += q * q;
        }
    }
    let norm = (energy_a * energy_b).sqrt();
    if norm <= 0.0 {
        return 0.0;
    }
    (dot / norm).clamp(-1.0, 1.0) as f32
}

/// The RMS of the band-by-band differences of two equal-length level lists, in dB. Every band
/// is first floored at [`TONE_RANGE_DB`] below the loudest band of the two lists, so near-silent
/// bands (leakage, rounding noise) do not dominate.
fn rms_diff(a: &[f32], b: &[f32]) -> f32 {
    let peak = a.iter().chain(b).copied().fold(f32::MIN, f32::max);
    rms_diff_above(a, b, peak - TONE_RANGE_DB)
}

/// The RMS of the band-by-band differences of two equal-length level lists, in dB, with every
/// band first raised to `floor`. [`rms_diff`] with the floor worked out by the caller, for
/// callers that compare the same frames many times.
pub(super) fn rms_diff_above(a: &[f32], b: &[f32], floor: f32) -> f32 {
    if a.is_empty() {
        return 0.0;
    }
    let sum: f32 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (x.max(floor) - y.max(floor)).powi(2))
        .sum();
    (sum / a.len() as f32).sqrt()
}

/// Third-octave band levels of a frame, from [`LOWEST_BAND_HZ`] up to Nyquist.
pub(super) struct Bands {
    /// Frame length.
    n: usize,
    fft: Fft,
    window: Vec<f32>,
    /// The band each FFT bin falls in; `None` below the lowest band.
    band_of_bin: Vec<Option<usize>>,
    /// The bands that hold at least one bin, in order. Empty bands say nothing.
    used: Vec<usize>,
}

impl Bands {
    /// The bands of an `n`-sample frame at `rate`.
    pub(super) fn new(n: usize, rate: u32) -> Bands {
        let bin_hz = f64::from(rate) / n as f64;
        let band_of_bin: Vec<Option<usize>> = (0..=n / 2)
            .map(|k| {
                let hz = k as f64 * bin_hz;
                (hz >= LOWEST_BAND_HZ).then(|| (3.0 * (hz / LOWEST_BAND_HZ).log2()) as usize)
            })
            .collect();
        let count = band_of_bin.iter().flatten().max().map_or(0, |&b| b + 1);
        let mut held = vec![false; count];
        band_of_bin.iter().flatten().for_each(|&b| held[b] = true);
        let used = (0..count).filter(|&b| held[b]).collect();
        Bands {
            n,
            fft: Fft::new(n),
            window: hann(n),
            band_of_bin,
            used,
        }
    }

    /// The level in dB of each used band for `audio[range]` (at most one frame), energy summed
    /// over channels. A short range is zero-padded after, or before when `pad_front`.
    pub(super) fn levels(
        &self,
        audio: &[Vec<f32>],
        range: Range<usize>,
        pad_front: bool,
    ) -> Vec<f32> {
        let mut energy = vec![0.0f64; self.used.last().map_or(0, |&b| b + 1)];
        // A unit sine on a bin gives |X| = n / 4, so this makes it 0 dB.
        let scale = (4.0 / self.n as f64).powi(2);
        for ch in audio {
            let part = &ch[range.clone()];
            let mut frame = vec![0.0f32; self.n];
            let at = if pad_front { self.n - part.len() } else { 0 };
            frame[at..at + part.len()].copy_from_slice(part);
            frame
                .iter_mut()
                .zip(&self.window)
                .for_each(|(v, w)| *v *= w);
            for (c, band) in self.fft.forward(&frame).iter().zip(&self.band_of_bin) {
                if let Some(b) = band {
                    energy[*b] += f64::from(c.norm_sqr()) * scale;
                }
            }
        }
        self.used
            .iter()
            .map(|&b| {
                let db = 10.0 * energy[b].log10();
                if db.is_finite() {
                    (db as f32).max(LEVEL_FLOOR_DB)
                } else {
                    LEVEL_FLOOR_DB
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rng::SplitMix64;
    use crate::loop_finder::fixtures::{RATE, TILE, noise, repeated, stereo_repeat};

    fn measure(audio: &[Vec<f32>], s: usize, e: usize) -> SeamMeasures {
        let natural = natural_change(audio, s, e, RATE);
        seam_measures(audio, s, e, RATE, &natural)
    }

    #[test]
    fn an_exact_repeat_is_seamless() {
        let a = stereo_repeat();
        let m = measure(&a, TILE, 3 * TILE - 1);
        assert!(m.click < 1e-6, "{m:?}");
        assert!(m.tone < 1e-3, "{m:?}");
        assert!(m.matching > 0.9999, "{m:?}");
        assert_eq!(quality(&m), Quality::Seamless);
    }

    #[test]
    fn a_steady_sine_looped_over_whole_periods_is_seamless() {
        // 220 Hz at 22 kHz is exactly 100 samples a period.
        let rate = 22_000;
        let sine: Vec<f32> = (0..2 * rate)
            .map(|i| (std::f32::consts::TAU * 220.0 * i as f32 / rate as f32).sin())
            .collect();
        let a = vec![sine];
        let (s, e) = (2_205, 2_205 + 100 * 100 - 1);
        let natural = natural_change(&a, s, e, rate);
        let m = seam_measures(&a, s, e, rate, &natural);
        assert_eq!(quality(&m), Quality::Seamless, "{m:?} {natural:?}");
    }

    #[test]
    fn a_hard_jump_is_rough() {
        // 220 Hz at 44 kHz is a period of exactly 200 samples: crests at 50 + 200k, troughs
        // at 150 + 200k.
        let rate = 44_000;
        let sine: Vec<f32> = (0..rate)
            .map(|i| (std::f32::consts::TAU * 220.0 * i as f32 / rate as f32).sin())
            .collect();
        let a = vec![sine];
        let (s, e) = (50 + 200 * 10, 150 + 200 * 20);
        let natural = natural_change(&a, s, e, rate);
        let m = seam_measures(&a, s, e, rate, &natural);
        assert!(m.click > 4.0, "{m:?}");
        assert_eq!(quality(&m), Quality::Rough);
    }

    #[test]
    fn noise_splices_are_never_better_than_fair() {
        let a = vec![noise(3, 3 * RATE as usize)];
        let len = a[0].len();
        let natural = natural_change(&a, 0, len - 1, RATE);
        let mut r = SplitMix64::new(4);
        for _ in 0..200 {
            let s = (r.next_u64() % (len as u64 / 2)) as usize;
            let first_e = s + 4410;
            let e = first_e + (r.next_u64() % (len - first_e) as u64) as usize;
            let m = seam_measures(&a, s, e, RATE, &natural);
            assert!(quality(&m) >= Quality::Fair, "({s}, {e}) {m:?}");
        }
    }

    #[test]
    fn out_of_phase_stereo_scores_like_mono() {
        let tile = noise(5, TILE);
        let mono = repeated(std::slice::from_ref(&tile));
        let flipped = repeated(&[tile.clone(), tile.iter().map(|v| -v).collect()]);
        // An exact repeat, and a splice that is off by 137 frames and so only partly matches.
        for (s, e) in [(TILE, 3 * TILE - 1), (TILE, 3 * TILE - 1 + 137)] {
            let a = measure(&mono, s, e).matching;
            let b = measure(&flipped, s, e).matching;
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
        let off = measure(&mono, TILE, 3 * TILE - 1 + 137).matching;
        assert!(off < 0.9, "{off}");
    }

    #[test]
    fn a_seam_broken_in_one_channel_is_not_seamless() {
        let mut a = stereo_repeat();
        let fresh = noise(6, 440);
        a[1][TILE - 220..TILE + 220].copy_from_slice(&fresh);
        let m = measure(&a, TILE, 3 * TILE - 1);
        assert_ne!(quality(&m), Quality::Seamless, "{m:?}");
        assert!(m.click > 0.1, "{m:?}");
    }

    #[test]
    fn a_loop_ending_at_the_last_frame_is_measured() {
        let a = stereo_repeat();
        let last = a[0].len() - 1;
        let m = measure(&a, TILE, last);
        assert!(m.click.is_finite() && m.tone.is_finite() && m.matching.is_finite());
        assert!(m.click < 1e-6, "{m:?}");
        assert!(m.matching > 0.9999, "{m:?}");
    }

    #[test]
    fn quality_thresholds() {
        let rows = [
            (SEAMLESS, Quality::Seamless, Quality::Good),
            (GOOD, Quality::Good, Quality::Fair),
            (FAIR, Quality::Fair, Quality::Rough),
        ];
        for (t, inside, outside) in rows {
            let on = SeamMeasures {
                click: t.click,
                tone: t.tone,
                matching: t.matching,
            };
            assert_eq!(quality(&on), inside, "{t:?}");
            // Just past the boundary in each measure, with the others still inside.
            let past = [
                SeamMeasures {
                    click: t.click * 1.01,
                    ..on
                },
                SeamMeasures {
                    tone: t.tone * 1.01,
                    ..on
                },
                SeamMeasures {
                    matching: t.matching - 1e-4,
                    ..on
                },
            ];
            for m in past {
                assert_eq!(quality(&m), outside, "{m:?}");
            }
        }
        let perfect = SeamMeasures {
            click: 0.0,
            tone: 0.0,
            matching: 1.0,
        };
        assert_eq!(quality(&perfect), Quality::Seamless);
        let nan = SeamMeasures {
            click: f32::NAN,
            ..perfect
        };
        assert_eq!(quality(&nan), Quality::Rough);
        assert!(Quality::Seamless < Quality::Good && Quality::Fair < Quality::Rough);
    }

    #[test]
    fn thresholds_get_looser_down_the_table() {
        for (a, b) in [(SEAMLESS, GOOD), (GOOD, FAIR)] {
            assert!(a.click < b.click && a.tone < b.tone && a.matching > b.matching);
        }
    }

    #[test]
    fn an_invalid_pair_is_the_worst_seam() {
        let a = stereo_repeat();
        let natural = natural_change(&a, 0, a[0].len() - 1, RATE);
        let real = measure(&a, TILE, 2 * TILE + 5);
        for (s, e) in [(100, 100), (200, 100), (100, a[0].len())] {
            let m = seam_measures(&a, s, e, RATE, &natural);
            assert_eq!(quality(&m), Quality::Rough, "({s}, {e})");
            assert_eq!(score(&m), f32::INFINITY, "({s}, {e})");
            assert!(score(&m).total_cmp(&score(&real)).is_gt());
        }
    }

    #[test]
    fn score_is_lower_for_better_seams() {
        let perfect = SeamMeasures {
            click: 0.0,
            tone: 0.0,
            matching: 1.0,
        };
        assert_eq!(score(&perfect), 0.0);
        let worse = SeamMeasures {
            click: 3.0,
            tone: 0.5,
            matching: 0.9,
        };
        let expect = 3.0f32.ln_1p() + 0.5 + 0.4;
        assert!((score(&worse) - expect).abs() < 1e-6);
        assert!(score(&worse) > score(&perfect));
    }
}
