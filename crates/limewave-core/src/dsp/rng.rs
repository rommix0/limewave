//! A small seeded random number generator, the only source of randomness in Limewave, so the
//! same input and settings always give the same samples.

use std::f32::consts::PI;

/// SplitMix64: a fast 64-bit generator with good statistical quality for audio use. The same
/// seed always gives the same sequence, on every platform.
#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// A generator starting from `seed`. Any value is fine, including 0.
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64 { state: seed }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform value in `0.0..1.0` (never 1.0), from the top 24 bits.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }

    /// A uniform phase in `−π..π` radians.
    pub fn phase(&mut self) -> f32 {
        (self.next_f32() * 2.0 - 1.0) * PI
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let take = |seed| {
            let mut r = SplitMix64::new(seed);
            (0..64).map(|_| r.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(take(7), take(7));
        assert_ne!(take(7), take(8));
        let mut r = SplitMix64::new(3);
        for _ in 0..10_000 {
            let f = r.next_f32();
            assert!((0.0..1.0).contains(&f));
            let p = r.phase();
            assert!((-PI..=PI).contains(&p));
        }
    }
}
