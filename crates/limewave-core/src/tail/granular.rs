//! The Granular method (v0.3 §4.3): continues the sound by quilting, stitching short patches of
//! the source region together, each chosen so it joins smoothly onto what came before.
//!
//! - The source is first divided by its smoothed level, so patches carry texture only; the
//!   tail's own envelope is applied afterwards (§4.7).
//! - Every channel uses the same patch plan, so the stereo image is kept.
//! - In Sustain the body is a seamless loop by construction (§4.8): its first patch starts at
//!   source position `o₁`, and its last patch ends exactly where `o₁` continues, so a wrap
//!   plays the source as it really went on. The release starts by playing that same wrap.

use std::f64::consts::TAU;

use super::{
    CUT_LEVEL_S, Generated, Resolved, TailAnalysis, TailError, TailMode, TailWarning,
    decay_envelope, end_ramp, join_overlap, secs_to_frames, tail_length,
};
use crate::dsp::rng::SplitMix64;
use crate::dsp::spectrum::{Complex32, Fft};
use crate::edit::EditState;

/// A tonal patch is about this many seconds, in whole periods...
const PATCH_TONAL_S: f64 = 0.040;

/// ...and at least this many periods.
const MIN_PATCH_PERIODS: usize = 4;

/// A tonal overlap is whole periods adding up to at least this many seconds.
const OVERLAP_TONAL_S: f64 = 0.005;

/// A noisy patch is this many seconds, or two of the strongest repeats if that is longer...
const PATCH_NOISY_S: f64 = 0.050;

/// ...and its overlap is a quarter of it.
const OVERLAP_NOISY_DIV: usize = 4;

/// A noisy sound's strongest repeat sizes the patch only when it is longer than this.
const SLOW_REPEAT_S: f64 = 0.008;

/// The level is smoothed over this many seconds, or two periods if that is longer.
const FLATTEN_S: f64 = 0.020;

/// The smoothed level never falls below this fraction of its peak, so near-silence is not
/// boosted into loud noise.
const FLATTEN_FLOOR: f32 = 1e-3;

/// The shortest join for a noisy sound when it must be shortened (a tonal one keeps a period).
const MIN_JOIN_NOISY_S: f64 = 0.002;

/// Subtracted from the score of a recently used position, fading over one patch length...
const REUSE_PENALTY: f32 = 0.15;

/// ...for this many of the most recent patches...
const REUSE_MEMORY: usize = 6;

/// ...and ignored beyond this many patch lengths away, where it is below 1e-6.
const REUSE_REACH: usize = 12;

/// The next patch is picked at random among this many of the best distinct candidates.
const CHOICES: usize = 3;

/// The seamless last patch tries this many steps either side of the length that fills the
/// body.
const LAST_PATCH_TRIES: isize = 2;

/// At full Darken the low-pass reaches this cutoff, in Hz, when the level is 60 dB down.
const DARK_END_HZ: f64 = 500.0;

/// Patch and overlap lengths in frames.
struct Sizes {
    patch: usize,
    overlap: usize,
    /// The period in whole frames, for a tonal source.
    period: Option<usize>,
    /// The strongest repeat of a noisy source, when it is long enough to matter.
    slow: Option<usize>,
    /// The source holds fewer than four patches: repetition may be audible.
    short: bool,
}

/// The patch and overlap lengths for a source region of `n` frames (v0.3 §4.3), shrunk when
/// two patches and their overlaps would not fit.
fn sizes(a: &TailAnalysis, n: usize, rate: u32) -> Sizes {
    let r = f64::from(rate);
    let period = a.character.period.map(|p| (p.round() as usize).max(1));
    let slow = a
        .character
        .strongest_lag
        .filter(|&l| l as f64 > SLOW_REPEAT_S * r);
    let (mut patch, mut overlap) = match period {
        Some(t) => {
            let k = ((PATCH_TONAL_S * r / t as f64).round() as usize).max(MIN_PATCH_PERIODS);
            let m = ((OVERLAP_TONAL_S * r / t as f64).ceil() as usize).max(1);
            (k * t, m * t)
        }
        None => {
            let p = ((PATCH_NOISY_S * r) as usize).max(2 * slow.unwrap_or(0));
            (p, p / OVERLAP_NOISY_DIV)
        }
    };
    let short = n < 4 * (patch + overlap);
    match period {
        // The source holds at least four periods, so one-period patches and overlaps fit.
        Some(t) => {
            while n < 2 * (patch + overlap) {
                if patch > 2 * overlap || (overlap == t && patch > t) {
                    patch -= t;
                } else if overlap > t {
                    overlap -= t;
                } else {
                    break;
                }
            }
        }
        None => {
            if n < 2 * (patch + overlap) {
                patch = n * 2 / 5;
                overlap = patch / OVERLAP_NOISY_DIV;
            }
        }
    }
    Sizes {
        patch: patch.max(2),
        overlap: overlap.max(1),
        period,
        slow,
        short,
    }
}

/// Divides each channel of `span` by the smoothed level (the RMS over channels in a centred
/// window of `win` frames, never below 1/1000 of its peak). Returns the flattened channels and
/// the level.
fn flatten(span: &[&[f32]], win: usize) -> (Vec<Vec<f32>>, Vec<f32>) {
    let n = span.first().map_or(0, |ch| ch.len());
    let mut prefix = vec![0.0f64; n + 1];
    for ch in span {
        for (i, &v) in ch.iter().enumerate() {
            prefix[i + 1] += f64::from(v) * f64::from(v);
        }
    }
    for i in 0..n {
        prefix[i + 1] += prefix[i];
    }
    let half = win / 2;
    let channels = span.len().max(1) as f64;
    let mut g: Vec<f32> = (0..n)
        .map(|i| {
            // The part of the window inside the region, so the ends are not pulled down.
            let lo = i.saturating_sub(half);
            let hi = (i + win - half).min(n);
            ((prefix[hi] - prefix[lo]) / ((hi - lo) as f64 * channels)).sqrt() as f32
        })
        .collect();
    let floor = (g.iter().copied().fold(0.0, f32::max) * FLATTEN_FLOOR).max(f32::MIN_POSITIVE);
    g.iter_mut().for_each(|v| *v = v.max(floor));
    let flat = span
        .iter()
        .map(|ch| ch.iter().zip(&g).map(|(v, l)| v / l).collect())
        .collect();
    (flat, g)
}

/// The fade-out and fade-in gains at frame `i` of an `n`-frame crossfade: linear for tonal
/// sounds, whose matched overlaps are correlated, and equal-power for noisy ones.
fn fade(i: usize, n: usize, tonal: bool) -> (f32, f32) {
    let t = (i as f64 + 0.5) / n as f64;
    if tonal {
        ((1.0 - t) as f32, t as f32)
    } else {
        let a = t * TAU / 4.0;
        (a.cos() as f32, a.sin() as f32)
    }
}

/// Chooses and stitches patches of the flattened source region.
struct Quilter<'a> {
    /// The flattened source region, one slice per channel.
    src: Vec<&'a [f32]>,
    patch: usize,
    overlap: usize,
    tonal: bool,
    /// `energy[i]`: the energy of the first `i` source frames, summed over channels.
    energy: Vec<f64>,
    fft: Fft,
    /// The spectrum of each source channel, for correlating a target against every position.
    spectra: Vec<Vec<Complex32>>,
    rng: SplitMix64,
    /// Source positions picked so far, latest last.
    recent: Vec<usize>,
}

impl<'a> Quilter<'a> {
    fn new(src: Vec<&'a [f32]>, sizes: &Sizes, seed: u64) -> Quilter<'a> {
        let n = src.first().map_or(0, |ch| ch.len());
        let mut energy = vec![0.0f64; n + 1];
        for ch in &src {
            for (i, &v) in ch.iter().enumerate() {
                energy[i + 1] += f64::from(v) * f64::from(v);
            }
        }
        for i in 0..n {
            energy[i + 1] += energy[i];
        }
        // At least n points, so no position's overlap wraps around.
        let fft = Fft::new(n.next_power_of_two());
        let spectra = src.iter().map(|ch| fft.forward(ch)).collect();
        Quilter {
            src,
            patch: sizes.patch,
            overlap: sizes.overlap,
            tonal: sizes.period.is_some(),
            energy,
            fft,
            spectra,
            rng: SplitMix64::new(seed),
            recent: Vec::new(),
        }
    }

    /// Normalizes `num` (Σ source × target over the overlap at `pos`, all channels) by the
    /// energies of the two.
    fn normalise(&self, num: f64, pos: usize, target_energy: f64) -> f32 {
        let e = self.energy[pos + self.overlap] - self.energy[pos];
        (num / ((e * target_energy).sqrt() + 1e-12)) as f32
    }

    fn target_energy(target: &[Vec<f32>]) -> f64 {
        target.iter().flatten().map(|&v| f64::from(v).powi(2)).sum()
    }

    /// How well the overlap at each source position `lo..=hi` matches `target` (an overlap's
    /// worth of frames per channel): the normalized cross-correlation summed over channels,
    /// from one FFT correlation for all positions.
    fn scores(&self, target: &[Vec<f32>], lo: usize, hi: usize) -> Vec<f32> {
        let mut acc = vec![Complex32::default(); self.fft.len() / 2 + 1];
        for (spec, t) in self.spectra.iter().zip(target) {
            for (a, (s, t)) in acc.iter_mut().zip(spec.iter().zip(self.fft.forward(t))) {
                *a += s * t.conj();
            }
        }
        let corr = self.fft.inverse(&acc);
        let te = Self::target_energy(target);
        (lo..=hi)
            .map(|pos| self.normalise(f64::from(corr[pos]), pos, te))
            .collect()
    }

    /// The score of one position, computed directly.
    fn score_at(&self, target: &[Vec<f32>], pos: usize) -> f32 {
        let num: f64 = self
            .src
            .iter()
            .zip(target)
            .map(|(ch, t)| {
                ch[pos..pos + self.overlap]
                    .iter()
                    .zip(t)
                    .map(|(a, b)| f64::from(*a) * f64::from(*b))
                    .sum::<f64>()
            })
            .sum();
        self.normalise(num, pos, Self::target_energy(target))
    }

    /// Lowers the scores (of positions from `lo`) near recently used positions, by
    /// `0.15 · e^(−distance / patch)`, so the tail does not keep returning to one place.
    fn penalise(&self, s: &mut [f32], lo: usize) {
        let Some(hi) = (lo + s.len()).checked_sub(1) else {
            return;
        };
        let decay = (-1.0 / self.patch as f64).exp() as f32;
        let reach = REUSE_REACH * self.patch;
        for &r in self.recent.iter().rev().take(REUSE_MEMORY) {
            // At and after r, then before it, fading by `decay` per frame.
            let from = r.max(lo);
            let mut w = REUSE_PENALTY * decay.powi((from - r).min(reach) as i32);
            for pos in from..=(r + reach).min(hi) {
                s[pos - lo] -= w;
                w *= decay;
            }
            if r > lo {
                let from = (r - 1).min(hi);
                let mut w = REUSE_PENALTY * decay.powi((r - from).min(reach) as i32);
                for pos in (r.saturating_sub(reach).max(lo)..=from).rev() {
                    s[pos - lo] -= w;
                    w *= decay;
                }
            }
        }
    }

    /// Picks the source position, in `lo..=hi`, of the patch to follow `target`: one of the
    /// best three candidates more than half an overlap apart, at random.
    fn pick(&mut self, target: &[Vec<f32>], lo: usize, hi: usize) -> usize {
        let mut s = self.scores(target, lo, hi);
        self.penalise(&mut s, lo);
        let apart = self.overlap / 2;
        let mut chosen: Vec<usize> = Vec::with_capacity(CHOICES);
        for _ in 0..CHOICES {
            let mut best: Option<usize> = None;
            for (k, &v) in s.iter().enumerate() {
                if chosen.iter().any(|&c| c.abs_diff(k) <= apart) {
                    continue;
                }
                // Ties go to the earlier position.
                if best.is_none_or(|b| v > s[b]) {
                    best = Some(k);
                }
            }
            match best {
                Some(k) => chosen.push(k),
                None => break,
            }
        }
        let k = chosen[(self.rng.next_u64() % chosen.len() as u64) as usize];
        self.recent.push(lo + k);
        lo + k
    }

    /// The last overlap's worth of `out`, per channel.
    fn tail_of(&self, out: &[Vec<f32>]) -> Vec<Vec<f32>> {
        out.iter()
            .map(|ch| ch[ch.len() - self.overlap..].to_vec())
            .collect()
    }

    /// Crossfades the end of `out` into the overlap at source position `pos`, then appends
    /// the `len` frames after that overlap.
    fn append(&self, out: &mut [Vec<f32>], pos: usize, len: usize) {
        let o = self.overlap;
        for (ch, src) in out.iter_mut().zip(&self.src) {
            let at = ch.len() - o;
            for i in 0..o {
                let (fo, fi) = fade(i, o, self.tonal);
                ch[at + i] = ch[at + i] * fo + src[pos + i] * fi;
            }
            ch.extend_from_slice(&src[pos + o..pos + o + len]);
        }
    }

    /// Adds one patch, searched in source positions `lo..=hi`.
    fn extend(&mut self, out: &mut [Vec<f32>], lo: usize, hi: usize) {
        let target = self.tail_of(out);
        let pos = self.pick(&target, lo, hi);
        self.append(out, pos, self.patch);
    }
}

/// Darken (v0.3 §4.7): a one-pole low-pass whose cutoff falls as the tail decays. It starts
/// at the sample rate, where it leaves the band practically untouched, and moves on a log scale
/// towards 500 Hz by the time the tail is 60 dB down (`t60` seconds in). Below full `amount` it
/// stops short, at `rate^(1 − amount) · 500^amount`.
fn darken(x: &mut [f32], amount: f32, t60: f32, rate: u32) {
    let Some(&first) = x.first() else {
        return;
    };
    if amount <= 0.0 {
        return;
    }
    let r = f64::from(rate);
    let (open, dark) = (r.ln(), DARK_END_HZ.min(r).ln());
    let span = f64::from(t60) * r;
    let mut z = first;
    for (i, v) in x.iter_mut().enumerate() {
        let prog = (i as f64 / span).min(1.0) * f64::from(amount);
        let cutoff = (open + (dark - open) * prog).exp();
        let a = (-TAU * cutoff / r).exp() as f32;
        z = (1.0 - a) * *v + a * z;
        *v = z;
    }
}

/// Renders the tail after `cut` by quilting patches of the analysed source region.
///
/// `SourceTooShort` when the region cannot hold a patch and two overlaps, which only happens
/// at very low sample rates, where 30 ms is a frame or two.
pub(super) fn render(
    audio: &[Vec<f32>],
    cut: usize,
    a: &TailAnalysis,
    s: &Resolved,
    e: &EditState,
    rate: u32,
) -> Result<Generated, TailError> {
    let (first, last) = a.source;
    let n = last + 1 - first;
    let sz = sizes(a, n, rate);
    let (p, o) = (sz.patch, sz.overlap);
    // The last position whose patch fits in the source, with room before it for an overlap.
    let hi = match n.checked_sub(o + p) {
        Some(hi) if hi >= o => hi,
        _ => return Err(TailError::SourceTooShort),
    };
    let tonal = sz.period.is_some();
    let mut warnings = Vec::new();
    if sz.short {
        warnings.push(TailWarning::ShortSource);
    }

    // Flatten from the source start up to the cut (a selection may end before it), so the
    // audio just before the cut can be matched too.
    let span: Vec<&[f32]> = audio.iter().map(|ch| &ch[first..cut]).collect();
    let m = cut - first;
    let win = secs_to_frames(FLATTEN_S, rate)
        .max(2 * sz.period.or(sz.slow).unwrap_or(0))
        .max(1);
    let (flat, mut level) = flatten(&span, win);
    // Land the level on the level at the cut over the last 20 ms: the tail starts there.
    let level0 = 10f32.powf(a.level_at_cut_db / 20.0);
    let n20 = secs_to_frames(CUT_LEVEL_S, rate).clamp(1, m);
    let from = level[m - n20];
    for k in 0..n20 {
        let t = if n20 == 1 {
            1.0
        } else {
            k as f32 / (n20 - 1) as f32
        };
        level[m - n20 + k] = from + (level0 - from) * t;
    }

    // The join before the cut, kept clear of an existing loop (v0.3 §4.6).
    let min_join = sz
        .period
        .unwrap_or_else(|| secs_to_frames(MIN_JOIN_NOISY_S, rate))
        .clamp(1, o);
    let join = match join_overlap(e, cut, o, min_join) {
        Some(j) => sz.period.map_or(j, |t| j / t * t),
        None => 0,
    };
    if join != o {
        warnings.push(TailWarning::OverlapShortened);
    }

    let mut q = Quilter::new(flat.iter().map(|ch| &ch[..n]).collect(), &sz, s.seed);
    // The flattened audio just before the cut: what the first patch must follow.
    let context: Vec<Vec<f32>> = flat.iter().map(|ch| ch[m - o..].to_vec()).collect();
    // Sustain leaves room before o₁ for the seamless last patch: up to 2 patches + 2 periods.
    let lo1 = match s.mode {
        TailMode::Decay => 0,
        TailMode::Sustain => (2 * p + 2 * sz.period.unwrap_or(p / 8)).min(hi),
    };
    let o1 = q.pick(&context, lo1, hi) + o;
    // The new audio after the cut, still flat, and the first patch's overlap for the join.
    let mut out: Vec<Vec<f32>> = q.src.iter().map(|ch| ch[o1..o1 + p].to_vec()).collect();
    let head: Vec<Vec<f32>> = q.src.iter().map(|ch| ch[o1 - join..o1].to_vec()).collect();

    let (body, loop_crossfade) = match s.mode {
        TailMode::Decay => {
            let len = tail_length(a.level_at_cut_db, s.decay_t60, rate, p);
            while out[0].len() < len {
                q.extend(&mut out, 0, hi);
            }
            let env = decay_envelope(level0, s.decay_t60, rate, len);
            for ch in &mut out {
                ch.truncate(len);
                ch.iter_mut().zip(&env).for_each(|(v, k)| *v *= k);
                darken(ch, s.darken, s.decay_t60, rate);
                end_ramp(ch, rate);
            }
            (0, 0)
        }
        TailMode::Sustain => {
            let plan = SustainPlan {
                o1,
                hi,
                level0,
                rate,
            };
            sustain(&mut q, &mut out, s, a, &sz, &plan, &mut warnings)
        }
    };

    // The join: the original fades out over the overlap as the first patch, at the landed
    // level, fades in.
    let audio = audio
        .iter()
        .zip(&head)
        .zip(out)
        .map(|((ch, head), new)| {
            let mut y = Vec::with_capacity(join + new.len());
            for (i, h) in head.iter().enumerate() {
                let (fo, fi) = fade(i, join, tonal);
                y.push(ch[cut - join + i] * fo + h * level[m - join + i] * fi);
            }
            y.extend(new);
            y
        })
        .collect();
    Ok(Generated {
        audio,
        overlap: join,
        body,
        loop_crossfade,
        warnings,
    })
}

/// Where the sustain body starts and what it may draw on.
struct SustainPlan {
    /// The source position the body's first patch continues from.
    o1: usize,
    /// The last source position whose patch fits.
    hi: usize,
    /// The level at the cut, which the body holds.
    level0: f32,
    rate: u32,
}

/// Sustain (v0.3 §3.2, §4.8): fills the body after the first patch (already in `out`, taken
/// from `o₁`), ends it with a patch that runs into `o₁` so the loop is seamless, and adds the
/// release, which starts with the audio a wrap would play. Applies the levels. Returns the body
/// length and the loop crossfade it needs (0 unless the seamless loop did not fit).
fn sustain(
    q: &mut Quilter,
    out: &mut [Vec<f32>],
    s: &Resolved,
    a: &TailAnalysis,
    sz: &Sizes,
    plan: &SustainPlan,
    warnings: &mut Vec<TailWarning>,
) -> (usize, usize) {
    let SustainPlan {
        o1,
        hi,
        level0,
        rate,
    } = *plan;
    let (p, o) = (sz.patch, sz.overlap);
    let len = |out: &[Vec<f32>]| out[0].len();
    while s.body.saturating_sub(len(out)) >= 2 * p {
        q.extend(out, 0, hi);
    }
    // The last patch ends at o₁, so the loop end runs into the loop start. Its length is near
    // what fills the body, in whole periods for a tonal sound, and the best matched is used.
    let step = sz.period.unwrap_or((p / 8).max(1)) as isize;
    let rest = s.body as isize - len(out) as isize;
    let rest = if rest >= 1 {
        rest
    } else {
        // Whole steps up to at least 1 frame (1 - rest is positive here).
        rest + step * ((1 - rest + step - 1) / step)
    };
    let target = q.tail_of(out);
    let mut best: Option<(f32, usize)> = None;
    for k in -LAST_PATCH_TRIES..=LAST_PATCH_TRIES {
        let Some(l) = usize::try_from(rest + k * step).ok().filter(|&l| l > 0) else {
            continue;
        };
        let Some(pos) = o1.checked_sub(l + o) else {
            continue;
        };
        let score = q.score_at(&target, pos);
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, l));
        }
    }
    let crossfade = match best {
        Some((_, l)) => {
            q.append(out, o1 - l - o, l);
            0
        }
        None => {
            // No room before o₁: an ordinary body, looped with a crossfade of one overlap.
            warnings.push(TailWarning::LoopCrossfadeFallback);
            while len(out) < s.body {
                q.extend(out, 0, hi);
            }
            out.iter_mut().for_each(|ch| ch.truncate(s.body));
            o
        }
    };
    let body = len(out);

    // The release: what a wrap plays (the source from o₁, the loop start's own audio) at the
    // body level until the first release patch joins, then a fade. With the crossfade
    // fallback too: the crossfade ends the loop on the audio just before the loop start, which
    // the source from o₁ continues.
    let mut hold = 0;
    if s.release_t60 > 0.0 {
        let fade_len = tail_length(a.level_at_cut_db, s.release_t60, rate, 1);
        for (ch, src) in out.iter_mut().zip(&q.src) {
            ch.extend_from_slice(&src[o1..o1 + p]);
        }
        hold = p - o;
        let total = body + hold + fade_len;
        while len(out) < total {
            q.extend(out, 0, hi);
        }
        out.iter_mut().for_each(|ch| ch.truncate(total));
    }
    let env = decay_envelope(level0, s.release_t60, rate, len(out) - body - hold);
    for ch in out.iter_mut() {
        ch[..body + hold].iter_mut().for_each(|v| *v *= level0);
        let release = &mut ch[body + hold..];
        release.iter_mut().zip(&env).for_each(|(v, k)| *v *= k);
        darken(release, s.darken, s.release_t60, rate);
        if ch.len() > body {
            end_ramp(&mut ch[body..], rate);
        }
    }
    (body, crossfade)
}

#[cfg(test)]
mod tests {
    use super::super::measure::*;
    use super::super::test_signals::*;
    use super::super::*;
    use crate::effects::apply_effect;

    fn decay() -> TailSettings {
        TailSettings::default()
    }

    fn sustain() -> TailSettings {
        TailSettings {
            mode: TailMode::Sustain,
            body_s: 1.0,
            ..TailSettings::default()
        }
    }

    #[test]
    fn granular_decay_join_is_click_free() {
        for (name, x, cut) in all() {
            let (r, out) = run(&x, &cut_at(&x, cut), &decay());
            let src = r.analysis.source;
            let at_cut = join_score(&out, cut, &x, src, RATE);
            let at_start = join_score(&out, r.step.first, &x, src, RATE);
            assert!(at_cut <= 1.5, "{name}: join at the cut {at_cut}");
            assert!(
                at_start <= 1.5,
                "{name}: join at the overlap start {at_start}"
            );
        }
    }

    #[test]
    fn granular_decay_starts_at_the_cut_level() {
        for (name, x, cut) in all() {
            let (r, out) = run(&x, &cut_at(&x, cut), &decay());
            let step = level_step_db(&out, cut, r.analysis.level_at_cut_db, RATE, tail_rate(&r));
            assert!(step.abs() <= 1.5, "{name}: level step {step} dB");
        }
    }

    #[test]
    fn granular_decay_follows_the_real_ending() {
        let (x, cut) = decaying_tone();
        let (_, out) = run(&x, &cut_at(&x, cut), &decay());
        let err = envelope_error_db(&out, cut, &x, cut, RATE);
        assert!(err <= 3.0, "envelope error {err} dB");
    }

    #[test]
    fn granular_decay_ends_in_silence() {
        for (name, x, cut) in all() {
            let (_, out) = run(&x, &cut_at(&x, cut), &decay());
            assert!(out.iter().all(|ch| ch[ch.len() - 1] == 0.0), "{name}");
            assert!(out[0].len() - cut <= at(30.0), "{name}");
        }
        // A very long decay stops at 30 s. A low rate and a short source keep this quick.
        let rate = 8000;
        let mut r = crate::dsp::rng::SplitMix64::new(2);
        let x = vec![(0..4000).map(|_| r.next_f32() - 0.5).collect::<Vec<f32>>()];
        let e = cut_at(&x, 4000);
        let s = TailSettings {
            decay_t60: Some(100.0),
            ..decay()
        };
        let t = render_tail(&x, &e, None, rate, &s).unwrap();
        let mut out = x.clone();
        apply_effect(&mut out, &t.step);
        assert_eq!(out[0].len() - 4000, 30 * rate as usize);
        assert_eq!(out[0][out[0].len() - 1], 0.0);
    }

    #[test]
    fn granular_sustain_loop_is_seamless() {
        for (name, x, cut) in all().into_iter().skip(1) {
            let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
            let ed = &r.edit;
            assert_eq!(ed.loop_mode, LoopMode::Forward, "{name}");
            assert_eq!(ed.crossfade, 0, "{name}");
            assert_eq!(ed.loop_start, cut, "{name}");
            assert!(r.warnings.is_empty(), "{name}: {:?}", r.warnings);
            let body = ed.loop_len();
            assert!(body.abs_diff(at(1.0)) < at(0.05), "{name}: body {body}");
            let wrap = wrap_score(
                &out,
                (ed.loop_start, ed.loop_end),
                &x,
                r.analysis.source,
                RATE,
            );
            assert!(wrap <= 1.5, "{name}: wrap {wrap}");
            let dev = level_deviation_db(
                &out,
                ed.loop_start,
                ed.loop_end,
                r.analysis.level_at_cut_db,
                RATE,
            );
            assert!(dev <= 1.5, "{name}: body level strays {dev} dB");
            let join = join_score(&out, cut, &x, r.analysis.source, RATE);
            assert!(join <= 1.5, "{name}: join {join}");
        }
    }

    #[test]
    fn sustain_with_loop_off_leaves_the_loop_alone() {
        let (x, cut) = steady_tone();
        let mut e = cut_at(&x, cut);
        e.loop_start = 1000;
        e.loop_end = 2000;
        let s = TailSettings {
            loop_on: false,
            ..sustain()
        };
        let (r, out) = run(&x, &e, &s);
        assert!(out[0].len() > cut + at(1.0));
        let ed = &r.edit;
        assert_eq!(
            (ed.loop_mode, ed.loop_start, ed.loop_end, ed.crossfade),
            (e.loop_mode, e.loop_start, e.loop_end, e.crossfade)
        );
    }

    #[test]
    fn granular_sustain_release_continues_the_wrap() {
        for (name, x, cut) in all().into_iter().skip(1) {
            let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
            let (a, b) = (r.edit.loop_start, r.edit.loop_end);
            assert!(out[0].len() > b + at(0.2), "{name}: there is a release");
            let same = (0..out[0].len() - b - 1)
                .take_while(|&i| out.iter().all(|ch| ch[b + 1 + i] == ch[a + i]))
                .count();
            // The release plays the wrap's own audio until its first patch joins.
            assert!(
                same >= at(0.02),
                "{name}: only {same} frames continue the wrap"
            );
            assert!(out.iter().all(|ch| ch[ch.len() - 1] == 0.0), "{name}");
        }
        // Release 0: the file ends at the loop end.
        let (x, cut) = steady_tone();
        let s = TailSettings {
            release_t60: Some(0.0),
            ..sustain()
        };
        let (r, out) = run(&x, &cut_at(&x, cut), &s);
        assert_eq!(out[0].len() - 1, r.edit.loop_end);
    }

    #[test]
    fn existing_loop_is_never_touched_in_decay() {
        for (name, x, cut) in all() {
            // The loop end right at the cut: no room at all; then 100 frames of room.
            for room in [0, 100] {
                let mut e = cut_at(&x, cut);
                e.loop_mode = LoopMode::Forward;
                e.loop_start = cut - at(0.3);
                e.loop_end = cut - 1 - room;
                e.crossfade = 64;
                let (r, out) = run(&x, &e, &decay());
                assert!(r.step.first > e.loop_end, "{name}, room {room}");
                for (o, i) in out.iter().zip(&x) {
                    assert_eq!(o[..=e.loop_end], i[..=e.loop_end], "{name}, room {room}");
                }
                assert!(
                    r.warnings.contains(&TailWarning::OverlapShortened),
                    "{name}, room {room}: {:?}",
                    r.warnings
                );
                let ed = &r.edit;
                assert_eq!(
                    (ed.loop_mode, ed.loop_start, ed.loop_end, ed.crossfade),
                    (e.loop_mode, e.loop_start, e.loop_end, e.crossfade)
                );
            }
        }
    }

    #[test]
    fn granular_keeps_stereo_correlation() {
        let (x, cut) = stereo_tone();
        for s in [decay(), sustain()] {
            let (r, out) = run(&x, &cut_at(&x, cut), &s);
            let (s0, s1) = r.analysis.source;
            let want = channel_correlation(&x, s0, s1);
            let got = channel_correlation(&out, cut, out[0].len() - 1);
            assert!((got - want).abs() <= 0.1, "{:?}: {got} vs {want}", s.mode);
        }
    }

    #[test]
    fn tail_is_deterministic() {
        for (name, x, cut, s) in [
            ("noise", noise(), decay()),
            ("stereo tone", stereo_tone(), sustain()),
        ]
        .map(|(name, (x, cut), s)| (name, x, cut, s))
        {
            let e = cut_at(&x, cut);
            let a = render_tail(&x, &e, None, RATE, &s).unwrap();
            let b = render_tail(&x, &e, None, RATE, &s).unwrap();
            assert_eq!(a, b, "{name}");
            let c = render_tail(&x, &e, None, RATE, &TailSettings { seed: 2, ..s }).unwrap();
            assert_ne!(a.step, c.step, "{name}: a new seed gives new audio");
        }
    }

    /// Prints the §8.1 numbers for the report: `cargo test -p limewave-core granular_numbers
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn granular_numbers() {
        for (name, x, cut) in all() {
            let (r, out) = run(&x, &cut_at(&x, cut), &decay());
            let src = r.analysis.source;
            println!(
                "{name} decay: join {:.2}, overlap start {:.2}, level step {:+.2} dB, tail {:.2} s, corr {:.3} vs {:.3}",
                join_score(&out, cut, &x, src, RATE),
                join_score(&out, r.step.first, &x, src, RATE),
                level_step_db(&out, cut, r.analysis.level_at_cut_db, RATE, tail_rate(&r)),
                (out[0].len() - cut) as f32 / RATE as f32,
                channel_correlation(&out, cut, out[0].len() - 1),
                channel_correlation(&x, src.0, src.1),
            );
            if name == "decaying tone" {
                println!(
                    "  envelope error {:.2} dB",
                    envelope_error_db(&out, cut, &x, cut, RATE)
                );
            }
            let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
            let ed = &r.edit;
            println!(
                "{name} sustain: join {:.2}, wrap {:.2}, body dev {:.2} dB, spread {:.2} dB, warnings {:?}",
                join_score(&out, cut, &x, src, RATE),
                wrap_score(&out, (ed.loop_start, ed.loop_end), &x, src, RATE),
                level_deviation_db(
                    &out,
                    ed.loop_start,
                    ed.loop_end,
                    r.analysis.level_at_cut_db,
                    RATE
                ),
                body_spread_db(&out, ed.loop_start, ed.loop_end, RATE),
                r.warnings,
            );
        }
    }

    #[test]
    fn darken_dulls_the_highs_but_not_the_join() {
        let (x, cut) = noise();
        let e = cut_at(&x, cut);
        let dark = TailSettings {
            darken: 1.0,
            ..decay()
        };
        let (r, out) = run(&x, &e, &dark);
        let (_, plain) = run(&x, &e, &decay());
        let join = join_score(&out, cut, &x, r.analysis.source, RATE);
        assert!(join <= 1.5, "join {join}");
        // Late in the tail the dark version has lost its highs: smaller steps for its level.
        let late = cut + at(0.9);
        let ratio = |a: &[Vec<f32>]| {
            let steps: f32 = (late..late + at(0.1))
                .map(|i| (a[0][i + 1] - a[0][i]).abs())
                .sum();
            let level: f32 = (late..late + at(0.1)).map(|i| a[0][i].abs()).sum();
            steps / level
        };
        assert!(
            ratio(&out) < 0.5 * ratio(&plain),
            "{} vs {}",
            ratio(&out),
            ratio(&plain)
        );
    }

    #[test]
    fn crossfade_fallback_release_continues_after_the_crossfade() {
        // 60 ms of noise leaves no room before o₁ for the seamless last patch. The noise is
        // heavily low-passed, so its steps are small and a jump between unrelated samples
        // shows as a click.
        let mut rng = crate::dsp::rng::SplitMix64::new(8);
        let mut z = 0.0f32;
        let smooth: Vec<f32> = (0..at(1.5))
            .map(|_| {
                z = 0.98 * z + 0.02 * (rng.next_f32() * 2.0 - 1.0);
                z
            })
            .collect();
        let (x, cut) = (vec![smooth], at(1.0));
        let e = cut_at(&x, cut);
        let n = at(0.06);
        let sel = Some((cut - n, cut - 1));
        let r = render_tail(&x, &e, sel, RATE, &sustain()).unwrap();
        assert!(
            r.warnings.contains(&TailWarning::LoopCrossfadeFallback),
            "{:?}",
            r.warnings
        );
        // The overlap of a shrunk noisy patch: a quarter of 2/5 of the source.
        let o = n * 2 / 5 / 4;
        let ed = &r.edit;
        assert_eq!((ed.loop_mode, ed.crossfade), (LoopMode::Forward, o));
        assert_eq!(ed.clone().clamped(x[0].len()), *ed);
        let mut out = x.clone();
        apply_effect(&mut out, &r.step);
        assert!(out[0].len() > ed.loop_end + at(0.2), "there is a release");

        // As played and saved: the loop crossfade applied, then the wrap and the release.
        let heard = crate::render::render(&out, ed);
        let src = r.analysis.source;
        let wrap = wrap_score(&heard, (ed.loop_start, ed.loop_end), &x, src, RATE);
        let release = join_score(&heard, ed.loop_end + 1, &x, src, RATE);
        assert!(wrap <= 1.5, "wrap {wrap}");
        assert!(release <= 1.5, "into the release {release}");
    }

    #[test]
    fn tiny_sources_at_very_low_rates_are_refused() {
        // 30 ms at 50 Hz is a frame and a half, so 2 or 3 frames pass the time check but
        // cannot hold a patch and two overlaps.
        for frames in [2, 3] {
            let x = vec![
                (0..frames)
                    .map(|i| 0.5 - 0.3 * i as f32)
                    .collect::<Vec<f32>>(),
            ];
            let e = EditState::default_for(frames);
            for mode in [TailMode::Decay, TailMode::Sustain] {
                for loop_on in [true, false] {
                    let s = TailSettings {
                        mode,
                        loop_on,
                        ..TailSettings::default()
                    };
                    assert_eq!(
                        render_tail(&x, &e, None, 50, &s),
                        Err(TailError::SourceTooShort),
                        "{frames} frames, {mode:?}"
                    );
                }
            }
        }
        // A few more frames render without a panic.
        let x = vec![
            (0..12)
                .map(|i| ((i * 7) % 5) as f32 * 0.2 - 0.4)
                .collect::<Vec<f32>>(),
        ];
        let e = EditState::default_for(12);
        for mode in [TailMode::Decay, TailMode::Sustain] {
            let s = TailSettings {
                mode,
                ..TailSettings::default()
            };
            if let Ok(r) = render_tail(&x, &e, None, 50, &s) {
                let Op::Splice(sp) = &r.step.op else {
                    panic!("not a splice")
                };
                assert!(sp.audio.iter().flatten().all(|v| v.is_finite()));
            }
        }
    }
}
