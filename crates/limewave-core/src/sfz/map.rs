//! Key-range helpers for instruments: Auto-map and Fill gaps.

/// Key ranges for regions with the given root keys, parallel to `roots`: each
/// runs from just above the midpoint to its lower neighbour up to the midpoint
/// to its upper neighbour (rounded into the lower region), the lowest reaching
/// key 0 and the highest key 127. Equal roots share a range.
pub fn auto_map(roots: &[u8]) -> Vec<(u8, u8)> {
    let mut sorted = roots.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mid = |a: u8, b: u8| ((a as u16 + b as u16) / 2) as u8;
    roots
        .iter()
        .map(|&r| {
            // `sorted` holds every root, so the search always hits.
            let i = sorted.binary_search(&r).unwrap_or(0);
            let lo = if i == 0 { 0 } else { mid(sorted[i - 1], r) + 1 };
            let hi = sorted.get(i + 1).map_or(127, |&next| mid(r, next));
            (lo.min(127), hi.min(127))
        })
        .collect()
}

/// The last key of the lower part when keys `a..=b` that nobody covers are
/// split in the middle, the lower part taking the middle key of an odd run.
fn gap_split(a: u8, b: u8) -> u8 {
    let len = b as u16 - a as u16 + 1;
    (a as u16 + len.div_ceil(2) - 1) as u8
}

/// Grows `ranges` (parallel output) to cover the whole keyboard: each run of
/// keys no range covers is split at its middle between the ranges on either
/// side, and at the ends of the keyboard the nearest range is extended.
/// Ranges never shrink. Inverted ranges are left as they are.
pub fn fill_gaps(ranges: &[(u8, u8)]) -> Vec<(u8, u8)> {
    let mut covered = [false; 128];
    for &(lo, hi) in ranges.iter().filter(|r| r.0 <= r.1) {
        for k in lo..=hi.min(127) {
            covered[k as usize] = true;
        }
    }
    ranges
        .iter()
        .map(|&(lo, hi)| {
            if lo > hi {
                return (lo, hi);
            }
            let (mut new_lo, mut new_hi) = (lo, hi);
            // A range covering the key below a gap ends exactly there, and one
            // covering the key above it starts exactly there.
            if hi < 127 && !covered[hi as usize + 1] {
                let b = (hi as usize + 1..128).find(|&k| covered[k]);
                new_hi = match b {
                    Some(next) => gap_split(hi + 1, next as u8 - 1),
                    None => 127,
                };
            }
            if lo > 0 && !covered[lo as usize - 1] {
                let a = (0..lo as usize).rev().find(|&k| covered[k]);
                new_lo = match a {
                    Some(prev) => gap_split(prev as u8 + 1, lo - 1) + 1,
                    None => 0,
                };
            }
            (new_lo, new_hi)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_map_splits_at_midpoints() {
        assert_eq!(auto_map(&[60]), [(0, 127)]);
        assert_eq!(auto_map(&[48, 60, 72]), [(0, 54), (55, 66), (67, 127)]);
        assert_eq!(auto_map(&[]), []);
    }

    #[test]
    fn auto_map_is_parallel_to_the_input_order() {
        assert_eq!(auto_map(&[72, 48, 60]), [(67, 127), (0, 54), (55, 66)]);
        // The midpoint rounds into the lower region.
        assert_eq!(auto_map(&[60, 61]), [(0, 60), (61, 127)]);
    }

    #[test]
    fn auto_map_shares_a_range_between_equal_roots() {
        assert_eq!(auto_map(&[60, 60]), [(0, 127), (0, 127)]);
        assert_eq!(
            auto_map(&[48, 60, 48, 60]),
            [(0, 54), (55, 127), (0, 54), (55, 127)]
        );
    }

    #[test]
    fn fill_gaps_splits_a_gap_at_its_middle() {
        assert_eq!(fill_gaps(&[(10, 20), (30, 40)]), [(0, 25), (26, 127)]);
        // Odd run 21..=23: the lower region takes the middle key.
        assert_eq!(fill_gaps(&[(10, 20), (24, 40)]), [(0, 22), (23, 127)]);
        // Even run 21..=24.
        assert_eq!(fill_gaps(&[(10, 20), (25, 40)]), [(0, 22), (23, 127)]);
    }

    #[test]
    fn fill_gaps_extends_the_nearest_region_at_the_ends() {
        assert_eq!(fill_gaps(&[(10, 20)]), [(0, 127)]);
        assert_eq!(fill_gaps(&[(0, 127)]), [(0, 127)]);
        assert_eq!(fill_gaps(&[]), []);
    }

    #[test]
    fn fill_gaps_never_shrinks() {
        let input = [(40, 80), (60, 100), (10, 50)];
        let out = fill_gaps(&input);
        for (a, b) in input.iter().zip(&out) {
            assert!(b.0 <= a.0 && b.1 >= a.1, "{a:?} -> {b:?}");
        }
        assert_eq!(out, [(40, 80), (60, 127), (0, 50)]);
    }

    #[test]
    fn fill_gaps_moves_every_region_on_a_boundary() {
        // Two regions end at 20; both reach the same middle.
        assert_eq!(
            fill_gaps(&[(10, 20), (5, 20), (30, 40)]),
            [(10, 25), (0, 25), (26, 127)]
        );
    }
}
