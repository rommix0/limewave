//! Upward zero-crossing search and loop-point snapping.

/// True when sample `i` is an upward zero crossing: negative before, non-negative at `i`.
pub fn is_upward(x: &[f32], i: usize) -> bool {
    i >= 1 && i < x.len() && x[i - 1] < 0.0 && x[i] >= 0.0
}

/// Closest upward crossing to `around` within `[around - window, around + window]`
/// (clamped to the signal); the lower index wins ties.
pub fn nearest_upward(x: &[f32], around: usize, window: usize) -> Option<usize> {
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
            .filter(|&i| is_upward(x, i))
            .or(above.filter(|&i| is_upward(x, i)))
    })
}

/// The next upward crossing strictly after `from` (forward) or strictly before it (backward).
pub fn step_upward(x: &[f32], from: usize, forward: bool) -> Option<usize> {
    if forward {
        (from.saturating_add(1)..x.len()).find(|&i| is_upward(x, i))
    } else {
        (1..from.min(x.len())).rev().find(|&i| is_upward(x, i))
    }
}

/// Moves a loop start onto the nearest upward crossing, or leaves it if none is in range.
pub fn snap_loop_start(x: &[f32], to: usize, window: usize) -> usize {
    nearest_upward(x, to, window).unwrap_or(to)
}

/// Moves an inclusive loop end so that `end + 1` lands on the nearest upward crossing,
/// or leaves it if none is in range.
pub fn snap_loop_end(x: &[f32], to: usize, window: usize) -> usize {
    match nearest_upward(x, to.saturating_add(1), window) {
        Some(c) => c - 1,
        None => to,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
