//! The Harmonic method (v0.3 §4.5): rebuilds the sound from its partials as smooth tones, plus
//! a quiet noise layer for what they leave over.
//!
//! - **Partials**: for a tonal source, one per harmonic of the refined fundamental (§4.4), each
//!   the strongest bin within ±¼ fundamental of its expected place, kept when it is a local
//!   peak within 70 dB of the loudest bin. For a noisy source, the prominent peaks of §4.4. At
//!   most 60, loudest first.
//! - **Frequencies**: a tonal source's partials are exact multiples of the fundamental; a noisy
//!   source's are their peaks' phase-vocoder frequencies. In Sustain the fundamental (or each
//!   peak) is rounded to whole cycles in the loop length L, so the body is exactly periodic
//!   (§4.8).
//! - **Amplitudes and phases**: a joint least-squares fit of every partial, per channel, over
//!   the last analysis frame (which ends at the cut), with constant amplitude, so each level is
//!   the frame's average, centred half a frame before the cut. The fit uses the unrounded
//!   frequencies, and each phase is carried to the cut, where the synthesis takes over.
//! - **Level**: each partial is carried from the frame's centre to the cut at its bin's
//!   measured rate, then fades at its bin's target rate (Decay), or holds (the Sustain body)
//!   and fades from the first frame after the loop end (the release).
//! - **Noise layer**: the fit's residual over that frame, frozen as a random-phase spectrum and
//!   synthesized as Smooth synthesizes its noise bins, at the per-bin rates; in Sustain its
//!   phases repeat every L.
//! - **Synthesis**: per sample, a sum of exponentially decaying cosines, the same in every
//!   channel apart from each channel's amplitude and phase, plus the noise layer. The join and
//!   the end are Smooth's.

use std::f64::consts::{PI, TAU};

use super::smooth::{
    self, Course, Frozen, analyse_spectrum, finish, first_guess, unit, whole_cycles,
};
use super::{Generated, Resolved, TailAnalysis, TailError, TailMode};
use crate::dsp::lsq::harmonic_fit;
use crate::dsp::spectrum::{Complex64, Fft, hann};
use crate::edit::EditState;

/// At most this many partials are synthesized, the loudest first.
const MAX_PARTIALS: usize = 60;

/// A tonal partial is searched for within this fraction of the fundamental either side of its
/// expected place...
const SEARCH_FRACTION: f64 = 0.25;

/// ...and kept when it is within this range of the loudest bin, in dB.
const PARTIAL_RANGE_DB: f32 = 70.0;

/// Each partial's oscillator is recomputed exactly every this many samples, so the recursive
/// rotation in between cannot drift.
const BLOCK: usize = 512;

/// One partial, ready to synthesize.
struct Partial {
    /// The frequency it is synthesized at, in radians per sample.
    omega: f64,
    /// Per channel: the complex amplitude at the cut, so the partial is
    /// `Re(c·e^{iω(t − cut)})` before any fade.
    coef: Vec<Complex64>,
    /// Its bin's measured fall, in dB per second.
    measured: f32,
    /// Its bin's target fall, in dB per second.
    target: f32,
}

/// What [`render`] synthesizes.
struct Plan {
    /// The frame size: the fit window, and the noise layer's frame.
    n: usize,
    /// The hop, a quarter frame.
    hop: usize,
    partials: Vec<Partial>,
    /// The residual, frozen with every bin noisy.
    noise: Frozen,
    course: Course,
    /// Frames of new audio after the cut.
    after: usize,
    /// The Sustain body length L (0 in Decay).
    body: usize,
}

/// The bins of a tonal source's partials (v0.3 §4.5): for each harmonic of `w0` (radians per
/// sample) below the Nyquist frequency, the strongest bin within ±¼ `w0` of its expected place
/// (the nearest bin when that range holds none), kept when it is a local peak within 70 dB of
/// the loudest bin and not already taken. Returns `(harmonic, bin)` pairs.
fn harmonic_bins(mag: &[f32], w0: f64, n: usize) -> Vec<(usize, usize)> {
    let bins = mag.len();
    if bins < 3 || w0.is_nan() || w0 <= 0.0 {
        return Vec::new();
    }
    let loudest = mag.iter().copied().fold(0.0f32, f32::max);
    let floor = loudest * 10f32.powf(-PARTIAL_RANGE_DB / 20.0);
    let f0 = w0 * n as f64 / TAU;
    let q = SEARCH_FRACTION * f0;
    let mut taken = vec![false; bins];
    let mut out = Vec::new();
    for h in (1..).take_while(|&h| h as f64 * w0 < PI) {
        let c = h as f64 * f0;
        let (mut lo, mut hi) = ((c - q).ceil(), (c + q).floor());
        if lo > hi {
            (lo, hi) = (c.round(), c.round());
        }
        let lo = (lo.max(1.0) as usize).min(bins - 2);
        let hi = (hi.max(0.0) as usize).min(bins - 2);
        if lo > hi {
            continue;
        }
        let i = (lo..=hi).fold(lo, |b, k| if mag[k] > mag[b] { k } else { b });
        if mag[i] > floor && mag[i] >= mag[i - 1] && mag[i] >= mag[i + 1] && !taken[i] {
            taken[i] = true;
            out.push((h, i));
        }
    }
    out
}

/// The partials' frequencies (v0.3 §4.5), loudest first and at most 60, as
/// `(fit, synthesis, bin)`: the frequency the fit uses and the one the synthesis uses (radians
/// per sample) and the bin whose rates it takes. They differ only in Sustain, where the
/// synthesis frequency completes whole cycles in `loop_len`.
fn partial_frequencies(
    sp: &smooth::Spectral,
    guess: Option<f64>,
    loop_len: Option<usize>,
) -> Vec<(f64, f64, usize)> {
    let mag = &sp.mag_last;
    let n = sp.n;
    let bins = mag.len();
    let nearest = |w: f64| ((w * n as f64 / TAU).round().max(0.0) as usize).min(bins - 1);
    let mut found: Vec<(f64, f64, usize, f32)> = match sp.w0.or(guess) {
        Some(w0) => {
            // The fundamental the synthesis uses: whole periods in L, and at least one.
            let ws = loop_len.map_or(w0, |l| whole_cycles(w0, l).max(TAU / l as f64));
            harmonic_bins(mag, w0, n)
                .into_iter()
                .map(|(h, i)| (h as f64 * w0, h as f64 * ws, i))
                .filter(|&(wf, ws, _)| wf < PI && ws < PI)
                .map(|(wf, ws, i)| (wf, ws, nearest(wf), mag[i]))
                .collect()
        }
        None => sp
            .peaks
            .iter()
            .map(|&p| {
                let wf = sp.raw[p].clamp(0.0, PI);
                let ws = loop_len.map_or(wf, |l| whole_cycles(wf, l));
                (wf, ws, nearest(wf), mag[p])
            })
            .filter(|&(_, ws, _, _)| ws <= PI)
            .collect(),
    };
    found.sort_by(|a, b| b.3.total_cmp(&a.3));
    found.truncate(MAX_PARTIALS);
    found
        .into_iter()
        .map(|(wf, ws, k, _)| (wf, ws, k))
        .collect()
}

/// Analyses the frame before the cut, fits the partials to it and freezes what they leave
/// over (v0.3 §4.5).
fn plan(
    audio: &[Vec<f32>],
    cut: usize,
    a: &TailAnalysis,
    s: &Resolved,
    rate: u32,
) -> Result<Plan, TailError> {
    let sp = analyse_spectrum(audio, cut, a, s, rate)?;
    let (n, hop) = (sp.n, sp.hop);
    let loop_len = (s.mode == TailMode::Sustain && sp.body > 0).then_some(sp.body);
    let freqs = partial_frequencies(&sp, first_guess(a), loop_len);
    let r = f64::from(rate.max(1));
    let hz: Vec<f64> = freqs.iter().map(|&(wf, _, _)| wf * r / TAU).collect();

    // The fit and its residual, per channel, over the frame ending at the cut.
    let fft = Fft::new(n);
    let w = hann(n);
    let mut coefs = Vec::with_capacity(audio.len());
    let mut mag = Vec::with_capacity(audio.len());
    let mut phasor = Vec::with_capacity(audio.len());
    for ch in audio {
        let seg = &ch[cut - n..cut];
        let c = harmonic_fit(seg, &hz, rate);
        let residual: Vec<f32> = seg
            .iter()
            .enumerate()
            .zip(&w)
            .map(|((t, &x), wi)| {
                let fitted: f64 = freqs
                    .iter()
                    .zip(&c)
                    .map(|(&(wf, _, _), c)| {
                        let (sin, cos) = (wf * t as f64).sin_cos();
                        c.re * cos - c.im * sin
                    })
                    .sum();
                ((f64::from(x) - fitted) * f64::from(*wi)) as f32
            })
            .collect();
        let spec = fft.forward(&residual);
        mag.push(spec.iter().map(|z| z.norm()).collect());
        phasor.push(spec.into_iter().map(unit).collect());
        // Each phase carried from the frame's start to the cut.
        coefs.push(
            freqs
                .iter()
                .zip(c)
                .map(|(&(wf, _, _), c)| c * Complex64::from_polar(1.0, wf * n as f64))
                .collect::<Vec<_>>(),
        );
    }
    let partials = freqs
        .iter()
        .enumerate()
        .map(|(j, &(_, ws, k))| Partial {
            omega: ws,
            coef: coefs.iter().map(|c| c[j]).collect(),
            measured: sp.measured[k],
            target: sp.target[k],
        })
        .collect();
    let bins = sp.mag_last.len();
    Ok(Plan {
        n,
        hop,
        partials,
        noise: Frozen {
            n,
            hop,
            mag,
            phasor,
            omega: vec![0.0; bins],
            noisy: vec![true; bins],
            measured: sp.measured,
            target: sp.target,
        },
        course: sp.course,
        after: sp.after,
        body: sp.body,
    })
}

/// Synthesizes the partials and the noise layer in Smooth's layout: index `n` is the cut, and
/// `n + after` frames per channel, of which the last hop before the cut and what follows are
/// meant to be used.
///
/// Each partial, before its fade, is `Re(c·e^{iω(i − n)})` at index `i`. In Sustain the
/// position after the cut is taken modulo the loop length, the level holds at the level
/// carried to the cut and, with a release, falls at the target rate from the frame after the
/// loop end.
fn synthesize(p: &Plan, course: Course, after: usize, rate: u32, seed: u64) -> Vec<Vec<f32>> {
    let mut out = smooth::synthesize(&p.noise, course, after, rate, seed);
    let n = p.n;
    let total = out.iter().map(Vec::len).min().unwrap_or(0);
    let r = f64::from(rate.max(1));
    let half = (n / 2) as f64;
    // The loop length, and the index from which the level falls at the target rate.
    let (wrap, fall_from) = match course {
        Course::Decay => (None, n),
        Course::Sustain { loop_len, release } => {
            let l = loop_len.max(1);
            (Some(l), if release { n + l } else { usize::MAX })
        }
    };
    // The position of index `i` relative to the cut, modulo the loop in Sustain.
    let pos = |i: usize| -> f64 {
        match wrap {
            Some(l) if i >= n => ((i - n) % l) as f64,
            _ => i as f64 - n as f64,
        }
    };
    let mut osc = vec![Complex64::default(); BLOCK];
    for q in &p.partials {
        // dB per sample.
        let (meas, targ) = (f64::from(q.measured) / r, f64::from(q.target) / r);
        // How far below the fitted level the partial is at index `i`, in dB.
        let db_at = |i: usize| -> f64 {
            match wrap {
                None => {
                    let t = i as f64 - n as f64;
                    meas * (t.min(0.0) + half) + targ * t.max(0.0)
                }
                Some(_) => meas * half + targ * i.saturating_sub(fall_from) as f64,
            }
        };
        let before = match wrap {
            None => meas,
            Some(_) => 0.0,
        };
        let step = |db: f64| Complex64::from_polar(10f64.powf(-db / 20.0), q.omega);
        let (rot_before, rot_after) = (step(before), step(targ));
        // Blocks of at most BLOCK samples, split where the level starts to fall, so each runs
        // at one rate: the oscillator for the block, then each channel's share of it.
        let mut start = 0;
        while start < total {
            let mut end = (start + BLOCK).min(total);
            if start < fall_from && fall_from < end {
                end = fall_from;
            }
            let rot = if start < fall_from {
                rot_before
            } else {
                rot_after
            };
            let mut u =
                Complex64::from_polar(10f64.powf(-db_at(start) / 20.0), q.omega * pos(start));
            for z in &mut osc[..end - start] {
                *z = u;
                u *= rot;
            }
            for (ch, c) in out.iter_mut().zip(&q.coef) {
                for (o, z) in ch[start..end].iter_mut().zip(&osc) {
                    *o += (c.re * z.re - c.im * z.im) as f32;
                }
            }
            start = end;
        }
    }
    out
}

/// Renders the tail after `cut` from the partials of the frame before it.
///
/// `SourceTooShort` when the region before the cut cannot hold two analysis frames, which only
/// happens at very low sample rates.
pub(super) fn render(
    audio: &[Vec<f32>],
    cut: usize,
    a: &TailAnalysis,
    s: &Resolved,
    e: &EditState,
    rate: u32,
) -> Result<Generated, TailError> {
    let p = plan(audio, cut, a, s, rate)?;
    let synth = synthesize(&p, p.course, p.after, rate, s.seed);
    Ok(finish(audio, cut, a, e, &synth, (p.n, p.hop), p.body, rate))
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::super::measure::*;
    use super::super::test_signals::*;
    use super::super::*;
    use super::{Course, plan, synthesize};

    fn decay() -> TailSettings {
        TailSettings {
            method: TailMethod::Harmonic,
            ..TailSettings::default()
        }
    }

    fn sustain() -> TailSettings {
        TailSettings {
            mode: TailMode::Sustain,
            body_s: 1.0,
            ..decay()
        }
    }

    #[test]
    fn harmonic_join_and_level() {
        for (name, x, cut) in all() {
            for s in [decay(), sustain()] {
                let (r, out) = run(&x, &cut_at(&x, cut), &s);
                let src = r.analysis.source;
                let mode = s.mode;
                let at_cut = join_score(&out, cut, &x, src, RATE);
                let at_start = join_score(&out, r.step.first, &x, src, RATE);
                assert!(at_cut <= 1.5, "{name} {mode:?}: join at the cut {at_cut}");
                assert!(
                    at_start <= 1.5,
                    "{name} {mode:?}: join at the overlap start {at_start}"
                );
                let fall = match mode {
                    TailMode::Decay => tail_rate(&r),
                    TailMode::Sustain => 0.0,
                };
                let step = level_step_db(&out, cut, r.analysis.level_at_cut_db, RATE, fall);
                assert!(step.abs() <= 1.5, "{name} {mode:?}: level step {step} dB");
                if mode == TailMode::Sustain {
                    let ed = &r.edit;
                    assert_eq!(ed.loop_mode, LoopMode::Forward, "{name}");
                    assert_eq!((ed.loop_start, ed.crossfade), (cut, 0), "{name}");
                    let wrap = wrap_score(&out, (ed.loop_start, ed.loop_end), &x, src, RATE);
                    assert!(wrap <= 1.5, "{name}: wrap {wrap}");
                    // The release starts with what a wrap plays.
                    let (a, b) = (ed.loop_start, ed.loop_end);
                    for ch in &out {
                        assert!((ch[b + 1] - ch[a]).abs() < 1e-5, "{name}");
                    }
                }
                assert!(out.iter().all(|ch| ch[ch.len() - 1] == 0.0), "{name}");
            }
        }
    }

    #[test]
    fn harmonic_sustain_keeps_its_shape() {
        let (x, cut) = steady_tone();
        let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
        let (a, b) = (r.edit.loop_start, r.edit.loop_end);
        let t = r.analysis.character.period.unwrap().round() as usize;
        let y = &out[0];
        let corr = |p: usize| {
            let (u, v) = (&y[p..p + t], &y[p + t..p + 2 * t]);
            let uv: f32 = u.iter().zip(v).map(|(a, b)| a * b).sum();
            let uu: f32 = u.iter().map(|a| a * a).sum();
            let vv: f32 = v.iter().map(|a| a * a).sum();
            uv / (uu * vv).sqrt()
        };
        let worst = (a..=b + 1 - 2 * t)
            .step_by(t)
            .map(corr)
            .fold(f32::MAX, f32::min);
        assert!(worst >= 0.98, "consecutive periods correlate at {worst}");

        let peak = |s: &[f32]| s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let (s0, s1) = r.analysis.source;
        let ratio_db = 20.0 * (peak(&y[a..=b]) / peak(&x[0][s0..=s1])).log10();
        assert!(
            ratio_db.abs() <= 3.0,
            "peak {ratio_db} dB from the source's"
        );
    }

    #[test]
    fn harmonic_sustain_body_holds_the_cut_level() {
        for (name, x, cut) in all() {
            let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
            let (a, b) = (r.edit.loop_start, r.edit.loop_end);
            // The general §8.1 bound. As for Smooth, the 20 ms level at the cut reads the steady
            // tone's 97 Hz pulse wave about 1.3 dB low, which uses most of this margin there.
            let dev = level_deviation_db(&out, a, b, r.analysis.level_at_cut_db, RATE);
            assert!(dev <= 1.5, "{name}: body level strays {dev} dB");
        }
    }

    #[test]
    fn harmonic_follows_the_real_ending() {
        let (x, cut) = decaying_tone();
        let (_, out) = run(&x, &cut_at(&x, cut), &decay());
        let err = envelope_error_db(&out, cut, &x, cut, RATE);
        assert!(err <= 3.0, "envelope error {err} dB");
    }

    #[test]
    fn harmonic_keeps_stereo_correlation() {
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
    fn harmonic_sustain_loop_is_exactly_periodic() {
        for (name, x, cut) in all() {
            let e = cut_at(&x, cut);
            let s = sustain();
            let a = analyse(&x, &e, None, RATE).unwrap();
            let resolved = resolve(&s, &a, RATE);
            let p = plan(&x, cut, &a, &resolved, RATE).unwrap();
            let (n, hop) = (p.n, p.hop);
            let l = p.body;
            assert_eq!(l % hop, 0, "{name}: L is whole hops");
            let course = Course::Sustain {
                loop_len: l,
                release: false,
            };
            let y = synthesize(&p, course, 2 * l, RATE, s.seed);
            for ch in &y {
                let worst = (0..l)
                    .map(|i| (ch[n + i] - ch[n + l + i]).abs())
                    .fold(0.0f32, f32::max);
                assert!(worst <= 1e-5, "{name}: halves differ by {worst}");
            }
            // The render's loop is that body.
            let r = render_tail(&x, &e, None, RATE, &s).unwrap();
            assert_eq!(r.edit.loop_len(), l, "{name}");
        }
    }

    #[test]
    fn harmonic_partials_are_exact_harmonics() {
        let (x, cut) = steady_tone();
        let e = cut_at(&x, cut);
        let a = analyse(&x, &e, None, RATE).unwrap();
        for s in [decay(), sustain()] {
            let p = plan(&x, cut, &a, &resolve(&s, &a, RATE), RATE).unwrap();
            assert_eq!(p.partials.len(), 20, "{:?}", s.mode);
            let w0 = p.partials.iter().map(|q| q.omega).fold(f64::MAX, f64::min);
            let f0 = w0 * f64::from(RATE) / TAU;
            assert!((f0 - 97.0).abs() < 0.5, "fundamental {f0} Hz");
            let mut hs: Vec<usize> = p
                .partials
                .iter()
                .map(|q| {
                    let h = q.omega / w0;
                    assert!((h - h.round()).abs() < 1e-9, "{h} is not a harmonic");
                    h.round() as usize
                })
                .collect();
            hs.sort_unstable();
            assert_eq!(hs, (1..=20).collect::<Vec<_>>());
            if s.mode == TailMode::Sustain {
                // A whole number of periods in L.
                let cycles = w0 * p.body as f64 / TAU;
                assert!((cycles - cycles.round()).abs() < 1e-9, "{cycles} cycles");
            }
        }

        // 200 harmonics of 50 Hz falling as 1/h: the 60 loudest are kept. Around the 60th,
        // neighbours differ by less than the window's scalloping (up to 1.4 dB), so only the
        // clearly louder ones are checked.
        let x = vec![
            (0..at(1.5))
                .map(|i| {
                    let t = i as f64 / f64::from(RATE);
                    (1..=200)
                        .map(|h| 0.1 / h as f64 * (TAU * 50.0 * h as f64 * t).cos())
                        .sum::<f64>() as f32
                })
                .collect::<Vec<f32>>(),
        ];
        let cut = at(1.0);
        let e = cut_at(&x, cut);
        let a = analyse(&x, &e, None, RATE).unwrap();
        assert!(a.character.period.is_some());
        let p = plan(&x, cut, &a, &resolve(&decay(), &a, RATE), RATE).unwrap();
        assert_eq!(p.partials.len(), 60);
        let w0 = p.partials.iter().map(|q| q.omega).fold(f64::MAX, f64::min);
        let hs: Vec<usize> = p
            .partials
            .iter()
            .map(|q| (q.omega / w0).round() as usize)
            .collect();
        assert!((1..=40).all(|h| hs.contains(&h)), "{hs:?}");
        assert!(hs.iter().all(|&h| h <= 70), "{hs:?}");
    }

    #[test]
    fn harmonic_on_noise_warns() {
        let (x, cut) = noise();
        for s in [decay(), sustain()] {
            let (r, _) = run(&x, &cut_at(&x, cut), &s);
            assert!(
                r.warnings.contains(&TailWarning::NoisySourceForMethod),
                "{:?}",
                r.warnings
            );
        }
        let (x, cut) = steady_tone();
        let (r, _) = run(&x, &cut_at(&x, cut), &decay());
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
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

    #[test]
    fn harmonic_never_touches_an_existing_loop() {
        for (name, x, cut) in all() {
            for room in [0, 100] {
                let mut e = cut_at(&x, cut);
                e.loop_mode = LoopMode::Forward;
                e.loop_start = cut - at(0.3);
                e.loop_end = cut - 1 - room;
                e.crossfade = 64;
                let (r, out) = run(&x, &e, &decay());
                assert_eq!(cut - r.step.first, room, "{name}: crossfades over the room");
                for (o, i) in out.iter().zip(&x) {
                    assert_eq!(o[..=e.loop_end], i[..=e.loop_end], "{name}, room {room}");
                }
                assert!(r.warnings.contains(&TailWarning::OverlapShortened));
            }
        }
    }

    #[test]
    fn tiny_sources_at_very_low_rates_are_refused_or_render() {
        for frames in [2, 3, 12, 40] {
            let x = vec![
                (0..frames)
                    .map(|i| ((i * 7) % 5) as f32 * 0.2 - 0.4)
                    .collect::<Vec<f32>>(),
            ];
            let e = EditState::default_for(frames);
            for s in [decay(), sustain()] {
                match render_tail(&x, &e, None, 50, &s) {
                    Ok(r) => {
                        let Op::Splice(sp) = &r.step.op else {
                            panic!("not a splice")
                        };
                        assert!(sp.audio.iter().flatten().all(|v| v.is_finite()));
                    }
                    Err(err) => {
                        assert_eq!(err, TailError::SourceTooShort, "{frames} frames");
                        assert!(frames <= 3, "{frames} frames refused");
                    }
                }
            }
        }
    }

    /// Prints the §8.1 numbers for the report: `cargo test -p limewave-core harmonic_numbers
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn harmonic_numbers() {
        for (name, x, cut) in all() {
            let (r, out) = run(&x, &cut_at(&x, cut), &decay());
            let src = r.analysis.source;
            println!(
                "{name} decay: join {:.2}, overlap start {:.2}, level step {:+.2} dB, tail {:.2} s, corr {:.3} vs {:.3}, envelope error {:.2} dB, warnings {:?}",
                join_score(&out, cut, &x, src, RATE),
                join_score(&out, r.step.first, &x, src, RATE),
                level_step_db(&out, cut, r.analysis.level_at_cut_db, RATE, tail_rate(&r)),
                (out[0].len() - cut) as f32 / RATE as f32,
                channel_correlation(&out, cut, out[0].len() - 1),
                channel_correlation(&x, src.0, src.1),
                envelope_error_db(&out, cut, &x, cut, RATE),
                r.warnings,
            );
            let (r, out) = run(&x, &cut_at(&x, cut), &sustain());
            let ed = &r.edit;
            println!(
                "{name} sustain: join {:.2}, overlap start {:.2}, level step {:+.2} dB, wrap {:.2}, body dev {:.2} dB, spread {:.2} dB, body {} frames",
                join_score(&out, cut, &x, src, RATE),
                join_score(&out, r.step.first, &x, src, RATE),
                level_step_db(&out, cut, r.analysis.level_at_cut_db, RATE, 0.0),
                wrap_score(&out, (ed.loop_start, ed.loop_end), &x, src, RATE),
                level_deviation_db(
                    &out,
                    ed.loop_start,
                    ed.loop_end,
                    r.analysis.level_at_cut_db,
                    RATE
                ),
                body_spread_db(&out, ed.loop_start, ed.loop_end, RATE),
                ed.loop_len(),
            );
        }
    }

    /// Times a 30 s stereo tail from a 1 s 48 kHz source: `cargo test --release -p
    /// limewave-core harmonic_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn harmonic_timing() {
        let rate = 48_000;
        let mut rng = crate::dsp::rng::SplitMix64::new(4);
        let x: Vec<Vec<f32>> = (0..2)
            .map(|c| {
                (0..rate as usize)
                    .map(|i| {
                        let t = i as f32 / rate as f32;
                        (1..=40)
                            .map(|h| {
                                0.3 / h as f32
                                    * (std::f32::consts::TAU * 110.0 * h as f32 * t + c as f32)
                                        .sin()
                            })
                            .sum::<f32>()
                            + 0.01 * (rng.next_f32() - 0.5)
                    })
                    .collect()
            })
            .collect();
        let e = EditState::default_for(x[0].len());
        for s in [
            TailSettings {
                decay_t60: Some(100.0),
                ..decay()
            },
            TailSettings {
                body_s: 30.0,
                release_t60: Some(0.0),
                ..sustain()
            },
        ] {
            let t = std::time::Instant::now();
            let r = render_tail(&x, &e, None, rate, &s).unwrap();
            let Op::Splice(sp) = &r.step.op else {
                panic!("not a splice")
            };
            println!(
                "{:?}: {:.2} s of tail in {:?}",
                s.mode,
                sp.audio[0].len() as f32 / rate as f32,
                t.elapsed()
            );
        }
    }
}
