//! Replayable effects: what the user picks (`Effect`), the resolved deterministic step that is
//! stored in the edit state (`AppliedEffect`), and the functions that plan, apply and replay them.

use crate::edit::{EditState, LoopMode};

/// The shape of a fade.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FadeCurve {
    Linear,
    Exponential,
    SCurve,
}

/// What Stereo to mono keeps: the left channel, the right channel, or the average of all
/// the channels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MonoSource {
    Left,
    Right,
    #[default]
    Mix,
}

/// An effect as the user picks it, before it is resolved against the audio.
#[derive(Clone, PartialEq, Debug)]
pub enum Effect {
    Gain { db: f32 },
    Normalize { target_db: f32 },
    FadeIn(FadeCurve),
    FadeOut(FadeCurve),
    Invert,
    RemoveDc,
    Reverse,
    StereoToMono(MonoSource),
    SwapChannels,
    MonoToStereo,
}

/// A resolved, deterministic step. Normalize becomes `Scale`; Remove DC becomes `Offset`.
#[derive(Clone, PartialEq, Debug)]
pub enum Op {
    /// Multiply by a factor.
    Scale(f32),
    /// Add one value per channel.
    Offset(Vec<f32>),
    FadeIn(FadeCurve),
    FadeOut(FadeCurve),
    Invert,
    /// Reverse the range; `mirror_markers` says the loop points are mirrored with it.
    Reverse {
        mirror_markers: bool,
    },
    StereoToMono(MonoSource),
    SwapChannels,
    MonoToStereo,
}

/// An operation with the inclusive frame range it works on.
#[derive(Clone, PartialEq, Debug)]
pub struct AppliedEffect {
    pub op: Op,
    pub first: usize,
    pub last: usize,
}

/// Number of frames in the audio (the first channel's length).
fn frame_count(audio: &[Vec<f32>]) -> usize {
    audio.first().map_or(0, Vec::len)
}

/// Orders a selection and clamps it to a file of `frames` (> 0) frames.
fn clamp_range(a: usize, b: usize, frames: usize) -> (usize, usize) {
    let last = frames - 1;
    (a.min(b).min(last), a.max(b).min(last))
}

/// Resolves what the user picked into a replayable step, using the range rules: a selection
/// (clamped to the file) wins for Gain, Normalize, Invert, Remove DC, the fades and Reverse;
/// without one, Gain, Normalize, Invert and Remove DC take the whole file and the fades and
/// Reverse take Start...End. Normalize and Remove DC measure the selection, or Start...End
/// when there is none. Channel tools ignore the selection and cover the whole file.
///
/// Returns `None` when there is nothing to do: no frames, a silent Normalize, or a channel
/// tool that does not fit the channel count.
pub fn plan_effect(
    effect: Effect,
    audio: &[Vec<f32>],
    e: &EditState,
    selection: Option<(usize, usize)>,
) -> Option<AppliedEffect> {
    let frames = frame_count(audio);
    if frames == 0 {
        return None;
    }
    let whole = (0, frames - 1);
    let selected = selection.map(|(a, b)| clamp_range(a, b, frames));
    // Start...End, kept inside the file and in order.
    let trim = clamp_range(e.start, e.end, frames);
    let channels = audio.len();

    let (op, (first, last)) = match effect {
        Effect::Gain { db } => {
            if db.is_nan() {
                return None;
            }
            let db = db.clamp(-48.0, 48.0);
            (Op::Scale(db_to_gain(db)), selected.unwrap_or(whole))
        }
        Effect::Normalize { target_db } => {
            let (m0, m1) = selected.unwrap_or(trim);
            let p = peak(audio, m0, m1);
            if !(p > 0.0 && p.is_finite() && target_db.is_finite()) {
                return None;
            }
            // A denormal peak can overflow the scale to infinity.
            let k = db_to_gain(target_db) / p;
            if !k.is_finite() {
                return None;
            }
            (Op::Scale(k), selected.unwrap_or(whole))
        }
        Effect::FadeIn(c) => (Op::FadeIn(c), selected.unwrap_or(trim)),
        Effect::FadeOut(c) => (Op::FadeOut(c), selected.unwrap_or(trim)),
        Effect::Invert => (Op::Invert, selected.unwrap_or(whole)),
        Effect::RemoveDc => {
            let (m0, m1) = selected.unwrap_or(trim);
            let offsets = audio
                .iter()
                .map(|ch| {
                    let last = ch.len() - 1;
                    -(mean(&ch[m0.min(last)..=m1.min(last)]) as f32)
                })
                .collect();
            (Op::Offset(offsets), selected.unwrap_or(whole))
        }
        Effect::Reverse => (
            Op::Reverse {
                mirror_markers: selected.is_none(),
            },
            selected.unwrap_or(trim),
        ),
        Effect::StereoToMono(pick) if channels >= 2 => (Op::StereoToMono(pick), whole),
        Effect::SwapChannels if channels >= 2 => (Op::SwapChannels, whole),
        Effect::MonoToStereo if channels == 1 => (Op::MonoToStereo, whole),
        Effect::StereoToMono(_) | Effect::SwapChannels | Effect::MonoToStereo => return None,
    };
    Some(AppliedEffect { op, first, last })
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Mean of a non-empty slice, summed in f64.
fn mean(s: &[f32]) -> f64 {
    s.iter().map(|&v| f64::from(v)).sum::<f64>() / s.len() as f64
}

/// Maps `t` in 0...1 to a fade gain in 0...1. The result is monotonic non-decreasing, and
/// exactly 0 at `t = 0` and exactly 1 at `t = 1`; `t` is clamped to that range.
///
/// - Linear: `t`.
/// - Exponential: `(e^(5t) - 1) / (e^5 - 1)`, a slow start and a steep finish (about -22 dB
///   at `t = 0.5`).
/// - S-curve: the smoothstep `3t^2 - 2t^3`, gentle at both ends.
pub fn fade_gain(curve: FadeCurve, t: f32) -> f32 {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    match curve {
        FadeCurve::Linear => t,
        FadeCurve::Exponential => {
            const K: f32 = 5.0;
            ((K * t).exp() - 1.0) / (K.exp() - 1.0)
        }
        FadeCurve::SCurve => t * t * (3.0 - 2.0 * t),
    }
}

/// Applies one step to the audio in place. The range is cut to the file, and a range that is
/// empty or starts past the end changes nothing, as in [`EditState::clamped`]; channel tools
/// ignore it. Every effect keeps the frame count.
pub fn apply_effect(audio: &mut Vec<Vec<f32>>, a: &AppliedEffect) {
    match &a.op {
        Op::StereoToMono(pick) => {
            if audio.len() >= 2 {
                let kept = match pick {
                    MonoSource::Left => audio.swap_remove(0),
                    MonoSource::Right => audio.swap_remove(1),
                    MonoSource::Mix => {
                        let n = audio.len() as f32;
                        (0..frame_count(audio))
                            .map(|i| {
                                audio
                                    .iter()
                                    .map(|ch| ch.get(i).copied().unwrap_or(0.0))
                                    .sum::<f32>()
                                    / n
                            })
                            .collect()
                    }
                };
                *audio = vec![kept];
            }
        }
        Op::SwapChannels => {
            if audio.len() >= 2 {
                audio.swap(0, 1);
            }
        }
        Op::MonoToStereo => {
            if audio.len() == 1 {
                audio.push(audio[0].clone());
            }
        }
        op => {
            for (c, ch) in audio.iter_mut().enumerate() {
                if a.first > a.last || a.first >= ch.len() {
                    continue;
                }
                let last = a.last.min(ch.len() - 1);
                apply_in_range(op, c, &mut ch[a.first..=last]);
            }
        }
    }
}

/// Applies a range op to one channel's slice of the range.
fn apply_in_range(op: &Op, channel: usize, s: &mut [f32]) {
    let n = s.len();
    // Position in the fade, 0...1; a single frame counts as the end.
    let t = |i: usize| {
        if n == 1 {
            1.0
        } else {
            (i as f64 / (n - 1) as f64) as f32
        }
    };
    match op {
        Op::Scale(k) => s.iter_mut().for_each(|v| *v *= k),
        Op::Offset(o) => {
            let d = o.get(channel).copied().unwrap_or(0.0);
            s.iter_mut().for_each(|v| *v += d);
        }
        Op::FadeIn(c) => s
            .iter_mut()
            .enumerate()
            .for_each(|(i, v)| *v *= fade_gain(*c, t(i))),
        Op::FadeOut(c) => s
            .iter_mut()
            .enumerate()
            .for_each(|(i, v)| *v *= fade_gain(*c, 1.0 - t(i))),
        Op::Invert => s.iter_mut().for_each(|v| *v = -*v),
        Op::Reverse { .. } => s.reverse(),
        Op::StereoToMono(_) | Op::SwapChannels | Op::MonoToStereo => {}
    }
}

/// Replays `effects` in order on a copy of the original audio.
pub fn process(original: &[Vec<f32>], effects: &[AppliedEffect]) -> Vec<Vec<f32>> {
    let mut audio = original.to_vec();
    for a in effects {
        apply_effect(&mut audio, a);
    }
    audio
}

/// The largest absolute sample value across all channels in `first..=last`; 0 for an empty
/// range.
pub fn peak(audio: &[Vec<f32>], first: usize, last: usize) -> f32 {
    audio
        .iter()
        .filter(|ch| first <= last && first < ch.len())
        .flat_map(|ch| &ch[first..=last.min(ch.len() - 1)])
        .fold(0.0, |m, v| f32::max(m, v.abs()))
}

/// Where the frame `f` of the original lands after the marker-mirroring reverses in
/// `effects`: each `Reverse { mirror_markers: true }` whose range contains the frame reflects
/// it to `first + last - f`, in order. Never overflows, whatever the ranges.
pub fn map_frame(effects: &[AppliedEffect], f: usize) -> usize {
    effects.iter().fold(f, |f, a| match a.op {
        // `last - (f - first)` is `first + last - f` without the overflowing sum.
        Op::Reverse {
            mirror_markers: true,
        } if (a.first..=a.last).contains(&f) => a.last - (f - a.first),
        _ => f,
    })
}

/// The edit state after a marker-mirroring reverse of `first..=last`: the loop points swap
/// ends and reflect (`new start = first + last - old end`). Start, End and everything else
/// stay as they are.
pub fn mirror_edit(e: &EditState, first: usize, last: usize) -> EditState {
    // `first + last - f` without the overflowing sum, saturating at both ends.
    let reflect = |f: usize| {
        if f >= first {
            last.saturating_sub(f - first)
        } else {
            last.saturating_add(first - f)
        }
    };
    EditState {
        loop_start: reflect(e.loop_end),
        loop_end: reflect(e.loop_start),
        ..e.clone()
    }
}

/// The first and last frame where any channel's absolute value is at least the threshold
/// (`threshold_db` dBFS), widened to keep the loop while it is on (start at most `loop_start`,
/// end at least `loop_end`). `None` when everything is below the threshold.
pub fn silence_bounds(
    audio: &[Vec<f32>],
    threshold_db: f32,
    e: &EditState,
) -> Option<(usize, usize)> {
    let frames = frame_count(audio);
    let threshold = db_to_gain(threshold_db);
    let loud = |i: &usize| {
        audio
            .iter()
            .any(|ch| ch.get(*i).is_some_and(|v| v.abs() >= threshold))
    };
    let first = (0..frames).find(loud)?;
    let last = (0..frames).rev().find(loud)?;
    if e.loop_mode == LoopMode::Off {
        Some((first, last))
    } else {
        Some((
            first.min(e.loop_start),
            last.max(e.loop_end).min(frames - 1),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVES: [FadeCurve; 3] = [FadeCurve::Linear, FadeCurve::Exponential, FadeCurve::SCurve];

    fn constant(frames: usize, v: f32) -> Vec<Vec<f32>> {
        vec![vec![v; frames]]
    }

    fn sine(frames: usize, period: f32, amp: f32, dc: f32) -> Vec<Vec<f32>> {
        vec![
            (0..frames)
                .map(|i| dc + amp * (std::f32::consts::TAU * i as f32 / period).sin())
                .collect(),
        ]
    }

    fn edit(frames: usize, start: usize, end: usize) -> EditState {
        EditState {
            start,
            end,
            ..EditState::default_for(frames)
        }
    }

    fn plan(
        effect: Effect,
        audio: &[Vec<f32>],
        e: &EditState,
        sel: Option<(usize, usize)>,
    ) -> AppliedEffect {
        plan_effect(effect, audio, e, sel).expect("effect should plan")
    }

    fn applied(audio: &[Vec<f32>], a: &AppliedEffect) -> Vec<Vec<f32>> {
        process(audio, std::slice::from_ref(a))
    }

    #[test]
    fn gain_scales_only_the_range() {
        let x = constant(100, 0.25);
        let e = EditState::default_for(100);
        let a = plan(Effect::Gain { db: 6.0206 }, &x, &e, Some((10, 19)));
        assert_eq!((a.first, a.last), (10, 19));
        let y = applied(&x, &a);
        for (i, v) in y[0].iter().enumerate() {
            let want = if (10..=19).contains(&i) { 0.5 } else { 0.25 };
            assert!((v - want).abs() < 1e-4, "frame {i}: {v}");
        }
    }

    #[test]
    fn gain_is_clamped_to_48_db() {
        let x = constant(10, 0.001);
        let e = EditState::default_for(10);
        let a = plan(Effect::Gain { db: 200.0 }, &x, &e, None);
        let Op::Scale(k) = a.op else {
            panic!("not a scale")
        };
        assert!((k - 10f32.powf(48.0 / 20.0)).abs() < 1.0);
    }

    #[test]
    fn normalize_hits_the_target_peak_and_scales_the_whole_file() {
        let x = sine(1000, 100.0, 0.25, 0.0);
        let e = edit(1000, 100, 899);
        let a = plan(Effect::Normalize { target_db: -0.1 }, &x, &e, None);
        assert!(matches!(a.op, Op::Scale(_)));
        assert_eq!((a.first, a.last), (0, 999));
        let target = 10f32.powf(-0.1 / 20.0);
        let k = target / peak(&x, 100, 899);
        let y = applied(&x, &a);
        assert!((peak(&y, 100, 899) - target).abs() < 1e-5);
        assert!((y[0][50] - x[0][50] * k).abs() < 1e-5);
    }

    #[test]
    fn normalize_of_silence_is_not_applied() {
        let x = constant(100, 0.0);
        let e = EditState::default_for(100);
        assert_eq!(
            plan_effect(Effect::Normalize { target_db: -0.1 }, &x, &e, None),
            None
        );
    }

    #[test]
    fn remove_dc_measures_start_to_end_and_shifts_the_whole_file() {
        let x = sine(1000, 100.0, 0.3, 0.1);
        let e = edit(1000, 100, 899);
        let a = plan(Effect::RemoveDc, &x, &e, None);
        assert_eq!((a.first, a.last), (0, 999));
        let Op::Offset(ref off) = a.op else {
            panic!("not an offset")
        };
        assert_eq!(off.len(), 1);
        let y = applied(&x, &a);
        let mean: f32 = y[0][100..=899].iter().sum::<f32>() / 800.0;
        assert!(mean.abs() < 1e-4, "mean {mean}");
        assert!((y[0][0] - (x[0][0] + off[0])).abs() < 1e-6);
        assert!((off[0] + 0.1).abs() < 1e-3);
    }

    #[test]
    fn invert_flips_the_sign() {
        let x = vec![vec![0.5, -0.25, 0.0, 1.0]];
        let e = EditState::default_for(4);
        let a = plan(Effect::Invert, &x, &e, Some((1, 3)));
        let y = applied(&x, &a);
        assert_eq!(y[0], vec![0.5, 0.25, 0.0, -1.0]);
    }

    #[test]
    fn fades_hit_their_end_points() {
        let x = constant(300, 0.3);
        let e = EditState::default_for(300);
        for c in CURVES {
            let y = applied(&x, &plan(Effect::FadeIn(c), &x, &e, Some((100, 199))));
            assert_eq!(y[0][100], 0.0, "{c:?} in start");
            assert_eq!(y[0][199], x[0][199], "{c:?} in end");
            assert_eq!(y[0][99], x[0][99]);
            assert_eq!(y[0][200], x[0][200]);
            let y = applied(&x, &plan(Effect::FadeOut(c), &x, &e, Some((100, 199))));
            assert_eq!(y[0][100], x[0][100], "{c:?} out start");
            assert_eq!(y[0][199], 0.0, "{c:?} out end");

            assert_eq!(fade_gain(c, 0.0), 0.0);
            assert_eq!(fade_gain(c, 1.0), 1.0);
            let mut prev = 0.0;
            for i in 0..=100 {
                let g = fade_gain(c, i as f32 / 100.0);
                assert!(g >= prev, "{c:?} not monotonic at {i}");
                prev = g;
            }
        }
    }

    #[test]
    fn ranges_without_a_selection_follow_the_rules() {
        let mono = constant(1000, 0.2);
        let stereo = vec![vec![0.2; 1000], vec![0.1; 1000]];
        let e = edit(1000, 100, 899);
        let range = |effect, audio: &[Vec<f32>], sel| {
            let a = plan(effect, audio, &e, sel);
            (a.first, a.last)
        };
        assert_eq!(range(Effect::Gain { db: 3.0 }, &mono, None), (0, 999));
        assert_eq!(range(Effect::Invert, &mono, None), (0, 999));
        assert_eq!(range(Effect::RemoveDc, &mono, None), (0, 999));
        assert_eq!(
            range(Effect::FadeIn(FadeCurve::Linear), &mono, None),
            (100, 899)
        );
        assert_eq!(
            range(Effect::FadeOut(FadeCurve::Linear), &mono, None),
            (100, 899)
        );
        assert_eq!(range(Effect::Reverse, &mono, None), (100, 899));
        assert_eq!(range(Effect::SwapChannels, &stereo, Some((5, 9))), (0, 999));
        assert_eq!(
            range(
                Effect::StereoToMono(MonoSource::Left),
                &stereo,
                Some((5, 9))
            ),
            (0, 999)
        );
        // A selection is clamped to the file and put in order.
        assert_eq!(range(Effect::Invert, &mono, Some((2000, 20))), (20, 999));
    }

    #[test]
    fn reverse_whole_mirrors_markers() {
        let x = vec![(0..1000).map(|i| i as f32 / 1000.0).collect::<Vec<f32>>()];
        let mut e = edit(1000, 100, 899);
        e.loop_mode = LoopMode::Forward;
        e.loop_start = 300;
        e.loop_end = 499;
        let a = plan(Effect::Reverse, &x, &e, None);
        assert_eq!(
            a,
            AppliedEffect {
                op: Op::Reverse {
                    mirror_markers: true
                },
                first: 100,
                last: 899
            }
        );
        let m = mirror_edit(&e, a.first, a.last);
        assert_eq!((m.loop_start, m.loop_end), (500, 699));
        assert_eq!((m.start, m.end), (100, 899));
        assert_eq!(m.loop_mode, LoopMode::Forward);
        let y = applied(&x, &a);
        for f in 100..=899 {
            assert_eq!(y[0][999 - f], x[0][f]);
        }
        assert_eq!(y[0][50], x[0][50]);
        assert_eq!(y[0][950], x[0][950]);
    }

    #[test]
    fn reverse_of_a_selection_does_not_mirror_markers() {
        let x = constant(1000, 0.1);
        let e = EditState::default_for(1000);
        let a = plan(Effect::Reverse, &x, &e, Some((200, 300)));
        assert_eq!(
            a.op,
            Op::Reverse {
                mirror_markers: false
            }
        );
        assert_eq!((a.first, a.last), (200, 300));
    }

    #[test]
    fn map_frame_follows_marker_mirrors() {
        let rev = AppliedEffect {
            op: Op::Reverse {
                mirror_markers: true,
            },
            first: 100,
            last: 899,
        };
        assert_eq!(map_frame(std::slice::from_ref(&rev), 300), 699);
        assert_eq!(map_frame(std::slice::from_ref(&rev), 50), 50);
        let twice = [rev.clone(), rev];
        for f in [0, 100, 300, 899, 950] {
            assert_eq!(map_frame(&twice, f), f);
        }
        let sel = AppliedEffect {
            op: Op::Reverse {
                mirror_markers: false,
            },
            first: 100,
            last: 899,
        };
        assert_eq!(map_frame(&[sel], 300), 300);
    }

    #[test]
    fn map_frame_never_overflows() {
        let wild = [AppliedEffect {
            op: Op::Reverse {
                mirror_markers: true,
            },
            first: 10,
            last: usize::MAX,
        }];
        assert_eq!(map_frame(&wild, usize::MAX - 1), 11);
        assert_eq!(map_frame(&wild, 10), usize::MAX);
        assert_eq!(map_frame(&wild, 5), 5);
    }

    #[test]
    fn mirror_edit_never_overflows() {
        let e = EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 20,
            loop_end: usize::MAX - 1,
            ..EditState::default_for(100)
        };
        let m = mirror_edit(&e, 10, usize::MAX);
        assert_eq!((m.loop_start, m.loop_end), (11, usize::MAX - 10));
        let low = EditState {
            loop_start: 5,
            loop_end: 30,
            ..e
        };
        let m = mirror_edit(&low, 10, usize::MAX);
        assert_eq!((m.loop_start, m.loop_end), (usize::MAX - 20, usize::MAX));
    }

    #[test]
    fn a_range_past_the_end_of_the_audio_changes_nothing() {
        let x = constant(3, 0.5);
        let mut y = x.clone();
        apply_effect(
            &mut y,
            &AppliedEffect {
                op: Op::Invert,
                first: usize::MAX,
                last: usize::MAX,
            },
        );
        assert_eq!(y, x);
    }

    #[test]
    fn normalize_of_a_denormal_peak_is_not_applied() {
        let x = vec![vec![f32::from_bits(1), 0.0, 0.0]];
        assert!(x[0][0] > 0.0 && x[0][0].is_subnormal());
        let e = EditState::default_for(3);
        assert_eq!(
            plan_effect(Effect::Normalize { target_db: -0.1 }, &x, &e, None),
            None
        );
    }

    #[test]
    fn channel_tools() {
        let stereo = vec![vec![0.2, 0.4, -0.6], vec![0.6, 0.0, -0.2]];
        let mono = vec![vec![0.1, 0.2, 0.3]];
        let e = EditState::default_for(3);

        let to_mono = |pick| {
            applied(
                &stereo,
                &plan(Effect::StereoToMono(pick), &stereo, &e, None),
            )
        };
        let y = to_mono(MonoSource::Mix);
        assert_eq!(y.len(), 1);
        for (got, want) in y[0].iter().zip([0.4, 0.2, -0.4]) {
            assert!((got - want).abs() < 1e-6);
        }
        assert_eq!(to_mono(MonoSource::Left), vec![stereo[0].clone()]);
        assert_eq!(to_mono(MonoSource::Right), vec![stereo[1].clone()]);
        // With more than two channels, Mix averages them all and Left and Right pick the
        // first two.
        let three = vec![vec![0.3], vec![0.6], vec![0.9]];
        let pick = |p| applied(&three, &plan(Effect::StereoToMono(p), &three, &e, None));
        assert!((pick(MonoSource::Mix)[0][0] - 0.6).abs() < 1e-6);
        assert_eq!(pick(MonoSource::Right), vec![vec![0.6]]);

        let y = applied(&stereo, &plan(Effect::SwapChannels, &stereo, &e, None));
        assert_eq!(y, vec![stereo[1].clone(), stereo[0].clone()]);

        let y = applied(&mono, &plan(Effect::MonoToStereo, &mono, &e, None));
        assert_eq!(y, vec![mono[0].clone(), mono[0].clone()]);

        assert_eq!(
            plan_effect(Effect::StereoToMono(MonoSource::Left), &mono, &e, None),
            None
        );
        assert_eq!(plan_effect(Effect::SwapChannels, &mono, &e, None), None);
        assert_eq!(plan_effect(Effect::MonoToStereo, &stereo, &e, None), None);
    }

    #[test]
    fn process_replays_in_order() {
        let x = vec![(0..50).map(|i| i as f32 / 100.0).collect::<Vec<f32>>()];
        let e = EditState::default_for(50);
        let gain = plan(Effect::Gain { db: -6.0 }, &x, &e, Some((0, 24)));
        let reverse = plan(Effect::Reverse, &x, &e, Some((10, 40)));
        let mut step = x.clone();
        apply_effect(&mut step, &gain);
        apply_effect(&mut step, &reverse);
        assert_eq!(process(&x, &[gain, reverse]), step);
        assert_eq!(process(&x, &[]), x);
    }

    #[test]
    fn empty_and_one_frame_audio_do_not_panic() {
        let all = [
            Effect::Gain { db: 3.0 },
            Effect::Normalize { target_db: -0.1 },
            Effect::FadeIn(FadeCurve::SCurve),
            Effect::FadeOut(FadeCurve::Exponential),
            Effect::Invert,
            Effect::RemoveDc,
            Effect::Reverse,
            Effect::StereoToMono(MonoSource::Mix),
            Effect::SwapChannels,
            Effect::MonoToStereo,
        ];
        let e = EditState::default_for(0);
        for audio in [vec![vec![]], vec![vec![], vec![]], vec![]] {
            for fx in &all {
                assert_eq!(plan_effect(fx.clone(), &audio, &e, None), None, "{fx:?}");
                assert_eq!(plan_effect(fx.clone(), &audio, &e, Some((3, 9))), None);
            }
        }

        let x = constant(1, 0.5);
        let e = EditState::default_for(1);
        for fx in [
            Effect::Gain { db: 3.0 },
            Effect::FadeIn(FadeCurve::Linear),
            Effect::FadeOut(FadeCurve::SCurve),
            Effect::Reverse,
        ] {
            let a = plan(fx, &x, &e, None);
            let y = applied(&x, &a);
            assert_eq!(y[0].len(), 1);
            assert!(y[0][0].is_finite());
        }
    }

    #[test]
    fn silence_bounds_respects_the_loop() {
        let mut x = vec![vec![0.0f32; 100]];
        x[0][30..=70].fill(0.5);
        let mut e = EditState::default_for(100);
        assert_eq!(silence_bounds(&x, -60.0, &e), Some((30, 70)));
        e.loop_mode = LoopMode::Forward;
        e.loop_start = 40;
        e.loop_end = 75;
        assert_eq!(silence_bounds(&x, -60.0, &e), Some((30, 75)));
        e.loop_start = 10;
        e.loop_end = 60;
        assert_eq!(silence_bounds(&x, -60.0, &e), Some((10, 70)));
        let zeros = constant(100, 0.0);
        assert_eq!(silence_bounds(&zeros, -60.0, &e), None);
    }

    #[test]
    fn silence_bounds_looks_at_every_channel() {
        let mut x = vec![vec![0.0f32; 50], vec![0.0f32; 50]];
        x[0][20] = 0.5;
        x[1][35] = -0.5;
        let e = EditState::default_for(50);
        assert_eq!(silence_bounds(&x, -60.0, &e), Some((20, 35)));
        // Below the threshold counts as silence.
        let quiet = constant(50, 0.0005);
        assert_eq!(silence_bounds(&quiet, -60.0, &e), None);
    }

    #[test]
    fn peak_is_the_largest_magnitude_across_channels() {
        let x = vec![vec![0.1, -0.7, 0.2], vec![0.3, 0.4, -0.9]];
        assert_eq!(peak(&x, 0, 2), 0.9);
        assert_eq!(peak(&x, 0, 1), 0.7);
        assert_eq!(peak(&x, 2, 1), 0.0);
        assert_eq!(peak(&[], 0, 5), 0.0);
    }
}
