//! Zero-crossing search and loop-point snapping, upward for forward loops and downward for backward ones.

use crate::edit::LoopMode;

/// Which way the signal crosses zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Crossing {
    /// Negative before, non-negative at the sample.
    Upward,
    /// Non-negative before, negative at the sample.
    Downward,
}

/// The crossing direction that makes a loop seam click-free for `mode`: backward loops play
/// the audio in reverse, so they want downward crossings.
pub fn loop_crossing(mode: LoopMode) -> Crossing {
    match mode {
        LoopMode::Backward => Crossing::Downward,
        _ => Crossing::Upward,
    }
}

/// True when sample `i` is a zero crossing of direction `c`.
pub fn is_crossing(x: &[f32], i: usize, c: Crossing) -> bool {
    if i < 1 || i >= x.len() {
        return false;
    }
    match c {
        Crossing::Upward => x[i - 1] < 0.0 && x[i] >= 0.0,
        Crossing::Downward => x[i - 1] >= 0.0 && x[i] < 0.0,
    }
}

/// True when sample `i` is an upward zero crossing: negative before, non-negative at `i`.
pub fn is_upward(x: &[f32], i: usize) -> bool {
    is_crossing(x, i, Crossing::Upward)
}

/// Closest crossing of direction `c` to `around` within `[around - window, around + window]`
/// (clamped to the signal); the lower index wins ties.
pub fn nearest_crossing(x: &[f32], around: usize, window: usize, c: Crossing) -> Option<usize> {
    if x.len() < 2 {
        return None;
    }
    let last = x.len() - 1;
    let lo = around.saturating_sub(window);
    let hi = around.saturating_add(window).min(last);
    if lo > hi {
        return None;
    }
    // Only distances that can land inside [lo, hi] need visiting (`around` may lie past the end).
    let max_d = around.abs_diff(lo).max(around.abs_diff(hi));
    (around.saturating_sub(hi)..=max_d).find_map(|d| {
        let below = around.checked_sub(d).filter(|&i| i >= lo && i <= hi);
        let above = around.checked_add(d).filter(|&i| i >= lo && i <= hi);
        below
            .filter(|&i| is_crossing(x, i, c))
            .or(above.filter(|&i| is_crossing(x, i, c)))
    })
}

/// Closest upward crossing to `around` within the window; see [`nearest_crossing`].
pub fn nearest_upward(x: &[f32], around: usize, window: usize) -> Option<usize> {
    nearest_crossing(x, around, window, Crossing::Upward)
}

/// The next crossing of direction `c` strictly after `from` (forward) or strictly before it
/// (backward).
pub fn step_crossing(x: &[f32], from: usize, forward: bool, c: Crossing) -> Option<usize> {
    if forward {
        (from.saturating_add(1)..x.len()).find(|&i| is_crossing(x, i, c))
    } else {
        (1..from.min(x.len())).rev().find(|&i| is_crossing(x, i, c))
    }
}

/// The next upward crossing strictly after `from` (forward) or strictly before it (backward).
pub fn step_upward(x: &[f32], from: usize, forward: bool) -> Option<usize> {
    step_crossing(x, from, forward, Crossing::Upward)
}

/// Moves a loop start onto the nearest crossing of direction `c`, or leaves it if none is in
/// range.
pub fn snap_loop_start_with(x: &[f32], to: usize, window: usize, c: Crossing) -> usize {
    nearest_crossing(x, to, window, c).unwrap_or(to)
}

/// Moves an inclusive loop end so that `end + 1` lands on the nearest crossing of direction
/// `c`, or leaves it if none is in range.
pub fn snap_loop_end_with(x: &[f32], to: usize, window: usize, c: Crossing) -> usize {
    match nearest_crossing(x, to.saturating_add(1), window, c) {
        Some(i) => i - 1,
        None => to,
    }
}

/// Moves a loop start onto the nearest upward crossing, or leaves it if none is in range.
pub fn snap_loop_start(x: &[f32], to: usize, window: usize) -> usize {
    snap_loop_start_with(x, to, window, Crossing::Upward)
}

/// Moves an inclusive loop end so that `end + 1` lands on the nearest upward crossing,
/// or leaves it if none is in range.
pub fn snap_loop_end(x: &[f32], to: usize, window: usize) -> usize {
    snap_loop_end_with(x, to, window, Crossing::Upward)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::seam_data_backward;

    /// Sine with a 100-sample period and a phase offset, so no sample is exactly zero.
    fn shifted_sine(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (2.0 * std::f32::consts::PI * i as f32 / 100.0 + 0.3).sin())
            .collect()
    }

    /// Sine with a 100-sample period, exactly 0.0 at every multiple of 100.
    fn sine(len: usize) -> Vec<f32> {
        (0..len)
            .map(|n| (2.0 * std::f32::consts::PI * (n % 100) as f32 / 100.0).sin())
            .collect()
    }

    #[test]
    fn upward_crossings_of_sine() {
        let x = sine(400);
        assert_eq!(nearest_upward(&x, 205, 30), Some(200));
        assert_eq!(nearest_upward(&x, 200, 0), Some(200));
        assert_eq!(nearest_upward(&x, 150, 30), None);
        // Equidistant: lower index wins.
        assert_eq!(nearest_upward(&x, 150, 50), Some(100));
        assert!(is_upward(&x, 100));
        assert!(!is_upward(&x, 0));
        assert!(!is_upward(&x, 50));
    }

    #[test]
    fn step_upward_moves_strictly() {
        let x = sine(400);
        assert_eq!(step_upward(&x, 100, true), Some(200));
        assert_eq!(step_upward(&x, 100, false), None);
        assert_eq!(step_upward(&x, 250, false), Some(200));
        assert_eq!(step_upward(&x, 399, true), None);
        assert_eq!(step_upward(&x, 1000, false), Some(300));
        assert_eq!(step_upward(&[], 0, true), None);
    }

    #[test]
    fn snap_loop_end_puts_next_sample_on_crossing() {
        let x = sine(400);
        assert_eq!(snap_loop_end(&x, 297, 30), 299);
        assert_eq!(snap_loop_start(&x, 297, 30), 300);
    }

    #[test]
    fn downward_crossings_of_sine() {
        let x = shifted_sine(500);
        let found: Vec<usize> = (0..x.len())
            .filter(|&i| is_crossing(&x, i, Crossing::Downward))
            .collect();
        assert_eq!(found.len(), 5);
        for &i in &found {
            assert!(x[i - 1] >= 0.0 && x[i] < 0.0, "i = {i}");
        }
        let (a, b) = (found[1], found[2]);
        assert_eq!(step_crossing(&x, a, true, Crossing::Downward), Some(b));
        assert_eq!(step_crossing(&x, b, false, Crossing::Downward), Some(a));
        assert_eq!(
            step_crossing(&x, *found.last().unwrap(), true, Crossing::Downward),
            None
        );
        // An upward crossing is not a downward one, and vice versa.
        let up = nearest_crossing(&x, 0, 100, Crossing::Upward).unwrap();
        assert!(!is_crossing(&x, up, Crossing::Downward));
        assert_eq!(
            nearest_crossing(&x, found[2] + 3, 10, Crossing::Downward),
            Some(found[2])
        );
    }

    #[test]
    fn backward_snap_gives_a_click_free_seam() {
        let x = shifted_sine(6000);
        let ls = snap_loop_start_with(&x, 1234, 60, Crossing::Downward);
        let le = snap_loop_end_with(&x, 4321, 60, Crossing::Downward);
        assert!(is_crossing(&x, ls, Crossing::Downward));
        assert!(is_crossing(&x, le + 1, Crossing::Downward));
        let s = seam_data_backward(&x, ls, le, 8);
        let max_step = x
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!((s.jump - s.natural_step.unwrap()).abs() <= max_step);
    }

    #[test]
    fn loop_crossing_follows_the_mode() {
        assert_eq!(loop_crossing(LoopMode::Backward), Crossing::Downward);
        for m in [LoopMode::Off, LoopMode::Forward, LoopMode::PingPong] {
            assert_eq!(loop_crossing(m), Crossing::Upward);
        }
    }

    #[test]
    fn no_crossing_returns_input() {
        let x = vec![0.5f32; 200];
        assert_eq!(snap_loop_start(&x, 50, 10), 50);
        assert_eq!(snap_loop_end(&x, 50, 10), 50);
    }

    #[test]
    fn crossing_search_on_short_inputs() {
        assert_eq!(nearest_upward(&[], 0, 10), None);
        assert_eq!(nearest_upward(&[1.0], 0, 10), None);
        assert_eq!(step_upward(&[1.0], 0, true), None);
        assert!(!is_upward(&[], 0));
        assert_eq!(snap_loop_start(&[], 5, 10), 5);
        // `around` far past the end must neither panic nor spin.
        let x = sine(400);
        assert_eq!(
            nearest_upward(&x, usize::MAX / 2, usize::MAX / 2),
            Some(300)
        );
        assert_eq!(nearest_upward(&x, usize::MAX, 10), None);
    }
}
