//! Min/max peak pyramid for fast, exact range queries.

/// Samples per block at level 0.
const BASE_BLOCK: usize = 64;

/// Levels of `(min, max)` pairs. Level 0 has one entry per 64 samples; each
/// further level halves the entry count.
pub struct PeakPyramid {
    levels: Vec<Vec<(f32, f32)>>,
    len: usize,
}

/// Average of the channels; empty input gives an empty vector.
pub fn mono_sum(audio: &[Vec<f32>]) -> Vec<f32> {
    let Some(len) = audio.iter().map(Vec::len).min() else {
        return Vec::new();
    };
    let n = audio.len() as f32;
    (0..len)
        .map(|i| audio.iter().map(|c| c[i]).sum::<f32>() / n)
        .collect()
}

fn merge((lo, hi): (f32, f32), (a, b): (f32, f32)) -> (f32, f32) {
    (lo.min(a), hi.max(b))
}

fn scan(x: &[f32]) -> (f32, f32) {
    x.iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |acc, &v| {
            merge(acc, (v, v))
        })
}

impl PeakPyramid {
    pub fn build(channel: &[f32]) -> PeakPyramid {
        let mut levels: Vec<Vec<(f32, f32)>> = Vec::new();
        if !channel.is_empty() {
            levels.push(channel.chunks(BASE_BLOCK).map(scan).collect());
            while levels.last().is_some_and(|l| l.len() > 1) {
                let next = levels[levels.len() - 1]
                    .chunks(2)
                    .map(|c| c.iter().copied().reduce(merge).unwrap_or((0.0, 0.0)))
                    .collect();
                levels.push(next);
            }
        }
        PeakPyramid {
            levels,
            len: channel.len(),
        }
    }

    /// Exact `(min, max)` over `[from, to_exclusive)`, clamped to the channel;
    /// `(0.0, 0.0)` for an empty range.
    pub fn min_max(&self, channel: &[f32], from: usize, to_exclusive: usize) -> (f32, f32) {
        let to = to_exclusive.min(self.len).min(channel.len());
        if from >= to {
            return (0.0, 0.0);
        }
        // Whole level-0 blocks inside the range; the ragged ends come from raw samples.
        let mut lo = from.div_ceil(BASE_BLOCK);
        let mut hi = to / BASE_BLOCK;
        if lo >= hi {
            return scan(&channel[from..to]);
        }
        let mut acc = merge(
            scan(&channel[from..lo * BASE_BLOCK]),
            scan(&channel[hi * BASE_BLOCK..to]),
        );
        // Climb the pyramid, taking odd edge blocks, so each level adds at most two.
        let mut level = 0;
        while lo < hi {
            if lo & 1 == 1 {
                acc = merge(acc, self.levels[level][lo]);
                lo += 1;
            }
            if hi & 1 == 1 {
                hi -= 1;
                acc = merge(acc, self.levels[level][hi]);
            }
            lo >>= 1;
            hi >>= 1;
            level += 1;
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny LCG so tests need no `rand` crate.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) as u32
        }
        fn next_f32(&mut self) -> f32 {
            self.next_u32() as f32 / u32::MAX as f32 * 2.0 - 1.0
        }
    }

    fn brute(x: &[f32], from: usize, to: usize) -> (f32, f32) {
        let to = to.min(x.len());
        if from >= to {
            return (0.0, 0.0);
        }
        x[from..to]
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            })
    }

    #[test]
    fn peaks_match_brute_force() {
        let mut rng = Lcg(12345);
        let x: Vec<f32> = (0..10_000).map(|_| rng.next_f32()).collect();
        let p = PeakPyramid::build(&x);
        for _ in 0..200 {
            let a = rng.next_u32() as usize % 10_000;
            let b = rng.next_u32() as usize % 10_000;
            let (from, to) = (a.min(b), a.max(b) + 1);
            assert_eq!(p.min_max(&x, from, to), brute(&x, from, to), "{from}..{to}");
        }
        // Exhaustive-ish: every short range around block boundaries, plus whole/overshoot.
        for from in [0, 1, 63, 64, 65, 127, 128, 4095, 4096, 9999] {
            for to in [0, 1, 64, 65, 128, 129, 5000, 8192, 10_000, 20_000] {
                assert_eq!(p.min_max(&x, from, to), brute(&x, from, to), "{from}..{to}");
            }
        }
    }

    #[test]
    fn peaks_empty_and_tiny_inputs() {
        let p = PeakPyramid::build(&[]);
        assert_eq!(p.min_max(&[], 0, 10), (0.0, 0.0));
        let one = [0.5f32];
        let p = PeakPyramid::build(&one);
        assert_eq!(p.min_max(&one, 0, 1), (0.5, 0.5));
        assert_eq!(p.min_max(&one, 0, 100), (0.5, 0.5));
        assert_eq!(p.min_max(&one, 1, 1), (0.0, 0.0));
        assert_eq!(p.min_max(&one, 5, 2), (0.0, 0.0));
    }

    #[test]
    fn mono_sum_averages_channels() {
        let audio = vec![vec![1.0, 0.0, -1.0], vec![0.0, 1.0, -1.0]];
        assert_eq!(mono_sum(&audio), vec![0.5, 0.5, -1.0]);
        assert!(mono_sum(&[]).is_empty());
    }
}
