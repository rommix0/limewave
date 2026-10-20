//! Loop crossfade rendering and seam data for the loop-boundary views.

use crate::edit::{EditState, FadeShape};

/// `(out_gain, in_gain)` for crossfade frame `i` of `n`, with `t = (i + 1) / n`.
pub fn fade_gains(shape: FadeShape, i: usize, n: usize) -> (f32, f32) {
    let t = (i + 1) as f32 / n as f32;
    match shape {
        FadeShape::Linear => (1.0 - t, t),
        FadeShape::EqualPower => {
            let a = t * std::f32::consts::FRAC_PI_2;
            (a.cos(), a.sin())
        }
    }
}

/// The crossfaded replacement for the frames ending at `loop_end`, as
/// `(first_frame, replacement_frames_per_channel)`.
///
/// `None` when the loop is off, the effective length `min(crossfade, max_crossfade)` is zero,
/// or the loop does not fit inside the audio. For `i in 0..N`:
/// `y[i] = out * x[loop_end - N + 1 + i] + in * x[loop_start - N + i]`.
pub fn crossfade_region(audio: &[Vec<f32>], e: &EditState) -> Option<(usize, Vec<Vec<f32>>)> {
    let n = e.crossfade.min(e.max_crossfade());
    let frames = audio.iter().map(Vec::len).min()?;
    if n == 0 || e.loop_end >= frames {
        return None;
    }
    let first = e.loop_end + 1 - n;
    let fade_in_first = e.loop_start - n;
    let replacement = audio
        .iter()
        .map(|ch| {
            (0..n)
                .map(|i| {
                    let (out, inn) = fade_gains(e.fade_shape, i, n);
                    out * ch[first + i] + inn * ch[fade_in_first + i]
                })
                .collect()
        })
        .collect();
    Some((first, replacement))
}

/// A full-length copy of `audio` with the crossfade region applied.
pub fn render(audio: &[Vec<f32>], e: &EditState) -> Vec<Vec<f32>> {
    let mut out = audio.to_vec();
    if let Some((first, replacement)) = crossfade_region(audio, e) {
        for (ch, rep) in out.iter_mut().zip(&replacement) {
            ch[first..first + rep.len()].copy_from_slice(rep);
        }
    }
    out
}

/// The samples around a loop seam on one channel.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SeamData {
    /// Up to `half` samples ending at `loop_end` inclusive.
    pub before_end: Vec<f32>,
    /// The samples just before `loop_start` (shorter at the file start).
    pub ghost_before_start: Vec<f32>,
    /// Up to `half` samples starting at `loop_start`.
    pub after_start: Vec<f32>,
    /// The samples just after `loop_end` (shorter at the file end).
    pub ghost_after_end: Vec<f32>,
    /// `x[loop_start] - x[loop_end]`: the step taken when the loop wraps.
    pub jump: f32,
    /// `x[loop_end + 1] - x[loop_end]` when that sample exists.
    pub natural_step: Option<f32>,
}

/// Seam data for one channel; empty with zero jump when the points are invalid for `x`.
pub fn seam_data(x: &[f32], loop_start: usize, loop_end: usize, half: usize) -> SeamData {
    if x.is_empty() || loop_end >= x.len() || loop_start > loop_end {
        return SeamData::default();
    }
    let before_from = (loop_end + 1).saturating_sub(half);
    let ghost_from = loop_start.saturating_sub(half);
    let after_to = (loop_start + half).min(x.len());
    let ghost_after_to = (loop_end + 1 + half).min(x.len());
    SeamData {
        before_end: x[before_from..=loop_end].to_vec(),
        ghost_before_start: x[ghost_from..loop_start].to_vec(),
        after_start: x[loop_start..after_to].to_vec(),
        ghost_after_end: x[loop_end + 1..ghost_after_to].to_vec(),
        jump: x[loop_start] - x[loop_end],
        natural_step: x.get(loop_end + 1).map(|next| next - x[loop_end]),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{EditState, FadeShape, LoopMode};
    use crate::testwav::sine;

    fn looped(frames: usize, ls: usize, le: usize, xf: usize, shape: FadeShape) -> EditState {
        EditState {
            loop_mode: LoopMode::Forward,
            loop_start: ls,
            loop_end: le,
            crossfade: xf,
            fade_shape: shape,
            ..EditState::default_for(frames)
        }
    }

    #[test]
    fn fade_gains_endpoints() {
        let n = 64;
        assert_eq!(fade_gains(FadeShape::Linear, n - 1, n), (0.0, 1.0));
        for i in 0..n {
            let (o, g) = fade_gains(FadeShape::EqualPower, i, n);
            assert!((o * o + g * g - 1.0).abs() < 1e-5, "i = {i}");
        }
    }

    #[test]
    fn crossfade_makes_seam_continuous() {
        let x = sine(441.0, 44100, 3000, 0.8);
        let e = looped(x.len(), 1000, 1149, 100, FadeShape::EqualPower);
        let audio = vec![x.clone()];
        let r = render(&audio, &e);
        assert_eq!(r[0].len(), x.len());
        assert!((r[0][1149] - x[999]).abs() < 1e-6);
        let max_step = x
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!((r[0][1000] - r[0][1149]).abs() <= max_step + 1e-6);
        // Frames outside the region are untouched.
        assert_eq!(r[0][..1050], x[..1050]);
        assert_eq!(r[0][1150..], x[1150..]);
    }

    #[test]
    fn crossfade_none_when_off_or_zero() {
        let audio = vec![sine(441.0, 44100, 2000, 0.5)];
        let mut e = looped(2000, 500, 1000, 100, FadeShape::Linear);
        e.loop_mode = LoopMode::Off;
        assert!(crossfade_region(&audio, &e).is_none());
        assert_eq!(render(&audio, &e), audio);
        let e = looped(2000, 500, 1000, 0, FadeShape::Linear);
        assert!(crossfade_region(&audio, &e).is_none());
        assert_eq!(render(&audio, &e), audio);
    }

    #[test]
    fn crossfade_clamped_to_available_audio() {
        let audio = vec![sine(441.0, 44100, 2000, 0.5)];
        let e = looped(2000, 20, 1000, 500, FadeShape::EqualPower);
        let (first, rep) = crossfade_region(&audio, &e).unwrap();
        assert_eq!(rep[0].len(), 20);
        assert_eq!(first, 1000 - 20 + 1);
        assert_eq!(render(&audio, &e)[0].len(), 2000);
    }

    #[test]
    fn crossfade_never_panics_on_unclamped_state() {
        let audio = vec![sine(441.0, 44100, 100, 0.5)];
        // loop_end past the audio
        assert!(crossfade_region(&audio, &looped(100, 20, 500, 10, FadeShape::Linear)).is_none());
        // inverted loop
        assert!(crossfade_region(&audio, &looped(100, 60, 30, 10, FadeShape::Linear)).is_none());
        assert_eq!(
            render(&audio, &looped(100, 20, 500, 10, FadeShape::Linear)),
            audio
        );
        assert!(crossfade_region(&[], &looped(100, 20, 50, 10, FadeShape::Linear)).is_none());
    }

    #[test]
    fn crossfade_applies_per_channel() {
        let l = sine(441.0, 44100, 2000, 0.5);
        let r = sine(330.0, 44100, 2000, 0.25);
        let audio = vec![l.clone(), r.clone()];
        let e = looped(2000, 600, 1000, 50, FadeShape::Linear);
        let (first, rep) = crossfade_region(&audio, &e).unwrap();
        assert_eq!(first, 951);
        assert_eq!(rep.len(), 2);
        for (ch, src) in [&l, &r].into_iter().enumerate() {
            for i in 0..50 {
                let (o, g) = fade_gains(FadeShape::Linear, i, 50);
                let want = o * src[951 + i] + g * src[550 + i];
                assert!((rep[ch][i] - want).abs() < 1e-6, "ch {ch} i {i}");
            }
        }
        assert_ne!(rep[0], rep[1]);
    }

    #[test]
    fn seam_data_known_values() {
        let mut x = vec![0.0f32; 3000];
        x[2915] = 8331.0 / 32768.0;
        x[2532] = 7381.0 / 32768.0;
        x[2916] = 7473.0 / 32768.0;
        let s = seam_data(&x, 2532, 2915, 64);
        assert!((s.jump - (7381.0 - 8331.0) / 32768.0).abs() < 1e-6);
        let step = s.natural_step.unwrap();
        assert!((step - (7473.0 - 8331.0) / 32768.0).abs() < 1e-6);
        assert_eq!(s.before_end.len(), 64);
        assert_eq!(*s.before_end.last().unwrap(), x[2915]);
        assert_eq!(s.after_start.len(), 64);
        assert_eq!(s.after_start[0], x[2532]);
        assert_eq!(s.ghost_before_start.len(), 64);
        assert_eq!(*s.ghost_before_start.last().unwrap(), x[2531]);
        assert_eq!(s.ghost_after_end.len(), 64);
        assert_eq!(s.ghost_after_end[0], x[2916]);
    }

    #[test]
    fn seam_data_at_file_edges() {
        let x: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let s = seam_data(&x, 0, 99, 16);
        assert!(s.ghost_before_start.is_empty());
        assert!(s.ghost_after_end.is_empty());
        assert_eq!(s.natural_step, None);
        assert_eq!(s.before_end.len(), 16);
        assert_eq!(s.after_start.len(), 16);
        assert_eq!(s.jump, -99.0);
    }

    #[test]
    fn seam_data_guards_bad_input() {
        let s = seam_data(&[], 0, 0, 8);
        assert!(s.before_end.is_empty() && s.after_start.is_empty());
        assert_eq!((s.jump, s.natural_step), (0.0, None));
        let x = [0.0f32; 10];
        assert_eq!(seam_data(&x, 2, 10, 4).jump, 0.0);
        assert_eq!(seam_data(&x, 5, 3, 4).natural_step, None);
    }
}
