//! The `smpl` chunk (sampler loops and pitch) and the conversion between a
//! root note plus cents and the chunk's unity-note/pitch-fraction fields.

use crate::error::WavError;

const HEADER_LEN: usize = 36;
const LOOP_LEN: usize = 24;
/// One semitone as a `u32` pitch fraction is `2^32`.
const FRACTION_SCALE: f64 = 4_294_967_296.0;

/// One loop record of a `smpl` chunk. `end` is inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmplLoop {
    pub cue_id: u32,
    pub loop_type: u32,
    pub start: u32,
    pub end: u32,
    pub fraction: u32,
    pub play_count: u32,
}

/// The contents of a `smpl` chunk. The loop count and sampler-data size are
/// derived from the vector lengths when writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Smpl {
    pub manufacturer: u32,
    pub product: u32,
    pub sample_period: u32,
    pub unity_note: u32,
    pub pitch_fraction: u32,
    pub smpte_format: u32,
    pub smpte_offset: u32,
    pub loops: Vec<SmplLoop>,
    pub sampler_data: Vec<u8>,
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

impl Smpl {
    /// Parses a `smpl` chunk body. Declared loop and sampler-data counts that
    /// exceed the body are clamped to what fits.
    pub fn parse(data: &[u8]) -> Result<Smpl, WavError> {
        if data.len() < HEADER_LEN {
            return Err(WavError::Malformed(format!(
                "smpl chunk is {} bytes, expected at least {HEADER_LEN}",
                data.len()
            )));
        }
        let fitting_loops = (data.len() - HEADER_LEN) / LOOP_LEN;
        let num_loops = (u32_at(data, 28) as usize).min(fitting_loops);
        let loops_end = HEADER_LEN + num_loops * LOOP_LEN;
        let sampler_len = (u32_at(data, 32) as usize).min(data.len() - loops_end);

        let loops = (0..num_loops)
            .map(|i| {
                let at = HEADER_LEN + i * LOOP_LEN;
                SmplLoop {
                    cue_id: u32_at(data, at),
                    loop_type: u32_at(data, at + 4),
                    start: u32_at(data, at + 8),
                    end: u32_at(data, at + 12),
                    fraction: u32_at(data, at + 16),
                    play_count: u32_at(data, at + 20),
                }
            })
            .collect();

        Ok(Smpl {
            manufacturer: u32_at(data, 0),
            product: u32_at(data, 4),
            sample_period: u32_at(data, 8),
            unity_note: u32_at(data, 12),
            pitch_fraction: u32_at(data, 16),
            smpte_format: u32_at(data, 20),
            smpte_offset: u32_at(data, 24),
            loops,
            sampler_data: data[loops_end..loops_end + sampler_len].to_vec(),
        })
    }

    /// Serialises to a `smpl` chunk body.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(HEADER_LEN + self.loops.len() * LOOP_LEN + self.sampler_data.len());
        for v in [
            self.manufacturer,
            self.product,
            self.sample_period,
            self.unity_note,
            self.pitch_fraction,
            self.smpte_format,
            self.smpte_offset,
            self.loops.len() as u32,
            self.sampler_data.len() as u32,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for l in &self.loops {
            for v in [
                l.cue_id,
                l.loop_type,
                l.start,
                l.end,
                l.fraction,
                l.play_count,
            ] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out.extend_from_slice(&self.sampler_data);
        out
    }
}

/// Converts a fraction of a semitone to a `u32` pitch fraction, saturating at
/// `u32::MAX`.
fn to_fraction(semitones: f64) -> u32 {
    (semitones * FRACTION_SCALE)
        .round()
        .clamp(0.0, u32::MAX as f64) as u32
}

/// Converts a root note and a tuning offset in cents into the `smpl`
/// `(unity_note, pitch_fraction)` pair. The fraction only goes upward, so a
/// negative offset becomes the note below plus a large fraction. A negative
/// offset on note 0 clamps to `(0, 0)`.
pub fn tuning_to_smpl(root: u8, cents: f32) -> (u32, u32) {
    let cents = cents as f64;
    if cents >= 0.0 {
        (root as u32, to_fraction(cents / 100.0))
    } else if root == 0 {
        (0, 0)
    } else {
        (root as u32 - 1, to_fraction((100.0 + cents) / 100.0))
    }
}

/// The pitch fraction of exactly half a semitone.
const HALF_SEMITONE: u32 = 1 << 31;

/// Converts `smpl` `(unity_note, pitch_fraction)` into the nearest root note
/// and a tuning offset in cents within `[-50, 50)`, except that exactly half a
/// semitone reads as `+50` on the unity note, so a saved `(n, +50)` comes back
/// unchanged. A unity note above 127 is clamped to 127.
pub fn smpl_to_tuning(unity: u32, fraction: u32) -> (u8, f32) {
    let unity = unity.min(127) as u8;
    if fraction == HALF_SEMITONE {
        return (unity, 50.0);
    }
    let cents = fraction as f64 / FRACTION_SCALE * 100.0;
    if cents >= 50.0 && unity < 127 {
        (unity + 1, (cents - 100.0) as f32)
    } else {
        let mut c = cents as f32;
        if c >= 50.0 {
            // Keep the half-open range when rounding to f32 reaches 50, or
            // when there is no higher note to move up to.
            c = f32::from_bits(50.0f32.to_bits() - 1);
        }
        (unity, c)
    }
}

/// The `smpl` sample period in nanoseconds for a sample rate.
pub fn sample_period(rate: u32) -> u32 {
    (1e9 / rate as f64).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(unity: u32, fraction: u32, period: u32, loops: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [0, 0, period, unity, fraction, 0, 0, loops.len() as u32, 0] {
            b.extend_from_slice(&u32::to_le_bytes(v));
        }
        for &(ty, start, end) in loops {
            for v in [0, ty, start, end, 0, 0] {
                b.extend_from_slice(&u32::to_le_bytes(v));
            }
        }
        b
    }

    #[test]
    fn parse_known_loop_file() {
        let s = Smpl::parse(&body(60, 0, 45351, &[(0, 2532, 2915)])).unwrap();
        assert_eq!(s.unity_note, 60);
        assert_eq!(s.pitch_fraction, 0);
        assert_eq!(s.sample_period, 45351);
        assert_eq!(s.loops.len(), 1);
        assert_eq!(s.loops[0].loop_type, 0);
        assert_eq!(s.loops[0].start, 2532);
        assert_eq!(s.loops[0].end, 2915);
        assert!(s.sampler_data.is_empty());
    }

    #[test]
    fn parse_zero_loops() {
        let b = body(60, 0, 45351, &[]);
        assert_eq!(b.len(), 36);
        assert!(Smpl::parse(&b).unwrap().loops.is_empty());
    }

    #[test]
    fn parse_clamps_overstated_counts() {
        let mut b = body(60, 0, 45351, &[(0, 1, 2)]);
        b[28..32].copy_from_slice(&9u32.to_le_bytes());
        b[32..36].copy_from_slice(&100u32.to_le_bytes());
        b.extend_from_slice(&[7, 8]);
        let s = Smpl::parse(&b).unwrap();
        assert_eq!(s.loops.len(), 1);
        assert_eq!(s.sampler_data, vec![7, 8]);
    }

    #[test]
    fn parse_too_short_is_malformed() {
        assert!(matches!(Smpl::parse(&[0; 35]), Err(WavError::Malformed(_))));
    }

    #[test]
    fn roundtrip_with_two_loops_and_sampler_data() {
        let s = Smpl {
            manufacturer: 1,
            product: 2,
            sample_period: 22676,
            unity_note: 48,
            pitch_fraction: 123_456,
            smpte_format: 3,
            smpte_offset: 4,
            loops: vec![
                SmplLoop {
                    cue_id: 0,
                    loop_type: 0,
                    start: 10,
                    end: 99,
                    fraction: 0,
                    play_count: 0,
                },
                SmplLoop {
                    cue_id: 1,
                    loop_type: 1,
                    start: 200,
                    end: 299,
                    fraction: 5,
                    play_count: 2,
                },
            ],
            sampler_data: vec![1, 2, 3, 4, 5],
        };
        assert_eq!(Smpl::parse(&s.to_bytes()).unwrap(), s);
    }

    #[test]
    fn tuning_positive_cents() {
        assert_eq!(tuning_to_smpl(48, 25.0), (48, 1_073_741_824));
    }

    #[test]
    fn tuning_negative_cents() {
        let expected = (0.88_f64 * 4_294_967_296.0).round() as u32;
        assert_eq!(tuning_to_smpl(48, -12.0), (47, expected));
    }

    #[test]
    fn tuning_roundtrip() {
        for root in 21u8..=108 {
            for cents in [-50.0f32, -12.5, 0.0, 12.5, 49.9, 50.0] {
                let (unity, fraction) = tuning_to_smpl(root, cents);
                let (r, c) = smpl_to_tuning(unity, fraction);
                assert!((-50.0..=50.0).contains(&c), "cents {c} out of range");
                let want = root as f64 + cents as f64 / 100.0;
                let got = r as f64 + c as f64 / 100.0;
                assert!((want - got).abs() < 1e-5, "{root} {cents}: {got} != {want}");
                if cents != -50.0 {
                    assert_eq!(r, root);
                    assert!((c - cents).abs() < 0.001);
                }
            }
        }
    }

    #[test]
    fn exact_half_semitone_reads_as_plus_50() {
        assert_eq!(tuning_to_smpl(60, 50.0), (60, 1 << 31));
        assert_eq!(smpl_to_tuning(60, 1 << 31), (60, 50.0));
        // Only the exact half reads as +50; just above it is the next note.
        let (r, c) = smpl_to_tuning(60, (1 << 31) + 1);
        assert_eq!(r, 61);
        assert!((c + 50.0).abs() < 1e-6, "{c}");
        let (r, c) = smpl_to_tuning(60, (1 << 31) - 1);
        assert_eq!(r, 60);
        assert!(c < 50.0, "{c}");
    }

    #[test]
    fn tuning_root_zero_negative_clamps() {
        assert_eq!(tuning_to_smpl(0, -10.0), (0, 0));
    }

    #[test]
    fn tuning_clamps_high_unity_and_stays_in_range() {
        assert_eq!(smpl_to_tuning(200, 0), (127, 0.0));
        let (r, c) = smpl_to_tuning(127, u32::MAX);
        assert_eq!(r, 127);
        assert!(c < 50.0);
    }

    #[test]
    fn sample_period_values() {
        assert_eq!(sample_period(22050), 45351);
        assert_eq!(sample_period(44100), 22676);
    }
}
