//! WAV sample format: parsing the `fmt ` chunk and converting between raw
//! sample bytes and planar `f32` audio.

use crate::error::WavError;

/// How each sample is stored. 8-bit integer samples are unsigned with an
/// offset of 128, as WAV defines; wider integers are signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Int { bits: u16 },
    Float { bits: u16 },
}

/// The audio parameters from a `fmt ` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavFormat {
    pub channels: u16,
    pub sample_rate: u32,
    pub format: SampleFormat,
    pub block_align: u16,
}

const TAG_PCM: u16 = 1;
const TAG_FLOAT: u16 = 3;
const TAG_EXTENSIBLE: u16 = 0xFFFE;

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// Parses the payload of a `fmt ` chunk (PCM, IEEE float or extensible).
pub fn parse_fmt(data: &[u8]) -> Result<WavFormat, WavError> {
    if data.len() < 16 {
        return Err(WavError::Malformed(format!(
            "fmt chunk is {} bytes, expected at least 16",
            data.len()
        )));
    }
    let mut tag = u16_at(data, 0);
    let channels = u16_at(data, 2);
    let sample_rate = u32_at(data, 4);
    let block_align = u16_at(data, 12);
    let container_bits = u16_at(data, 14);
    let mut bits = container_bits;

    if tag == TAG_EXTENSIBLE {
        if data.len() < 40 {
            return Err(WavError::Malformed(
                "extensible fmt chunk is shorter than 40 bytes".into(),
            ));
        }
        let valid = u16_at(data, 18);
        if valid != 0 {
            bits = valid;
        }
        tag = u16_at(data, 24);
    }
    if bits != container_bits {
        return Err(WavError::UnsupportedFormat(format!(
            "{bits} valid bits in a {container_bits}-bit container"
        )));
    }
    let format = match (tag, bits) {
        (TAG_PCM, 8 | 16 | 24 | 32) => SampleFormat::Int { bits },
        (TAG_FLOAT, 32 | 64) => SampleFormat::Float { bits },
        (TAG_PCM | TAG_FLOAT, _) => {
            return Err(WavError::UnsupportedFormat(format!(
                "{bits}-bit samples with format tag {tag}"
            )));
        }
        _ => {
            return Err(WavError::UnsupportedFormat(format!(
                "format tag {tag} (only PCM and IEEE float are supported)"
            )));
        }
    };
    if channels == 0 {
        return Err(WavError::Malformed("fmt chunk has zero channels".into()));
    }
    if block_align == 0 {
        return Err(WavError::Malformed("fmt chunk has zero block align".into()));
    }
    Ok(WavFormat {
        channels,
        sample_rate,
        format,
        block_align,
    })
}

fn is_supported(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::Int {
            bits: 8 | 16 | 24 | 32
        } | SampleFormat::Float { bits: 32 | 64 }
    )
}

fn bytes_per_sample(format: SampleFormat) -> usize {
    match format {
        SampleFormat::Int { bits } | SampleFormat::Float { bits } => usize::from(bits / 8),
    }
}

/// Bytes per frame: the declared block align, but never less than the samples need.
pub fn frame_stride(fmt: &WavFormat) -> usize {
    usize::from(fmt.block_align).max(usize::from(fmt.channels) * bytes_per_sample(fmt.format))
}

/// A copy of the `fmt ` payload `fmt` for `channels` channels: the channel count, the block
/// align (`channels` × container bytes) and the byte rate (`rate` × block align) are updated.
/// An extensible chunk (tag 0xFFFE, at least 24 bytes) also gets the channel mask for 1 channel
/// (`0x4`, front centre) or 2 (`0x3`, front left and right). A payload shorter than 16 bytes
/// is returned unchanged.
pub fn fmt_with_channels(fmt: &[u8], channels: u16) -> Vec<u8> {
    let mut out = fmt.to_vec();
    if out.len() < 16 {
        return out;
    }
    let block_align = channels.saturating_mul(u16_at(&out, 14) / 8);
    let byte_rate = u32_at(&out, 4).saturating_mul(u32::from(block_align));
    out[2..4].copy_from_slice(&channels.to_le_bytes());
    out[8..12].copy_from_slice(&byte_rate.to_le_bytes());
    out[12..14].copy_from_slice(&block_align.to_le_bytes());
    if u16_at(&out, 0) == TAG_EXTENSIBLE && out.len() >= 24 {
        let mask = match channels {
            1 => Some(0x4u32),
            2 => Some(0x3),
            _ => None,
        };
        if let Some(mask) = mask {
            out[20..24].copy_from_slice(&mask.to_le_bytes());
        }
    }
    out
}

/// A copy of the `fmt ` payload `fmt` for sampling rate `rate`: the rate and the byte rate
/// (`rate` × block align, saturating) are updated and everything else stays. A payload shorter
/// than 16 bytes is returned unchanged.
pub fn fmt_with_rate(fmt: &[u8], rate: u32) -> Vec<u8> {
    let mut out = fmt.to_vec();
    if out.len() < 16 {
        return out;
    }
    let byte_rate = rate.saturating_mul(u32::from(u16_at(&out, 12)));
    out[4..8].copy_from_slice(&rate.to_le_bytes());
    out[8..12].copy_from_slice(&byte_rate.to_le_bytes());
    out
}

/// A plain 16-byte `fmt ` payload for `fmt`: tag 1 (PCM) or 3 (float), channels, rate, byte
/// rate, block align and bits per sample.
pub fn fmt_bytes(fmt: &WavFormat) -> Vec<u8> {
    let (tag, bits) = match fmt.format {
        SampleFormat::Int { bits } => (TAG_PCM, bits),
        SampleFormat::Float { bits } => (TAG_FLOAT, bits),
    };
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&fmt.channels.to_le_bytes());
    out.extend_from_slice(&fmt.sample_rate.to_le_bytes());
    out.extend_from_slice(&(fmt.sample_rate * u32::from(fmt.block_align)).to_le_bytes());
    out.extend_from_slice(&fmt.block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out
}

/// Reads one sample from exactly `bytes_per_sample` bytes of a supported format.
fn decode_sample(b: &[u8], format: SampleFormat) -> f32 {
    match format {
        SampleFormat::Int { bits: 8 } => (f32::from(b[0]) - 128.0) / 128.0,
        SampleFormat::Int { bits: 16 } => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0,
        SampleFormat::Int { bits: 24 } => {
            (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0
        }
        SampleFormat::Int { bits: 32 } => {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0
        }
        SampleFormat::Float { bits: 32 } => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFormat::Float { bits: 64 } => {
            f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32
        }
        _ => 0.0,
    }
}

/// Decodes interleaved sample bytes to planar `[channel][frame]` audio. A
/// partial trailing frame is dropped. An unsupported format (which
/// `parse_fmt` never produces) decodes to empty channels.
pub fn decode(data: &[u8], fmt: &WavFormat) -> Vec<Vec<f32>> {
    let channels = usize::from(fmt.channels);
    if !is_supported(fmt.format) || channels == 0 {
        return vec![Vec::new(); channels];
    }
    let bps = bytes_per_sample(fmt.format);
    let stride = frame_stride(fmt);
    (0..channels)
        .map(|ch| {
            let samples = data
                .chunks_exact(stride)
                .map(|f| &f[ch * bps..(ch + 1) * bps]);
            // One loop per format, so the format is not matched again for every sample.
            match fmt.format {
                SampleFormat::Int { bits: 16 } => samples
                    .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                    .collect(),
                SampleFormat::Float { bits: 32 } => samples
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect(),
                format => samples.map(|b| decode_sample(b, format)).collect(),
            }
        })
        .collect()
}

/// Scales to `2^(bits-1)`, rounds to nearest and clips to the signed range.
fn encode_int(x: f32, bits: u16) -> i64 {
    let scale = f64::from(1u32 << (bits - 1));
    let v = f64::from(x) * scale;
    // Rounds half away from zero like `f64::round`, without its slow library call. An `f32`
    // times a power of two has at most 24 significant bits, so adding 0.5 is exact and the
    // truncating cast lands on the same integer.
    let r = if v < 0.0 { v - 0.5 } else { v + 0.5 } as i64;
    r.clamp(-(scale as i64), scale as i64 - 1)
}

/// Encodes planar audio as interleaved sample bytes, rounding to nearest and
/// clipping to the integer range (no dither). The frame count is that of the
/// shortest channel.
pub fn encode_frames(audio: &[Vec<f32>], fmt: &WavFormat) -> Vec<u8> {
    if !is_supported(fmt.format) {
        return Vec::new();
    }
    let frames = audio.iter().map(Vec::len).min().unwrap_or(0);
    let bps = bytes_per_sample(fmt.format);
    let stride = frame_stride(fmt).max(audio.len() * bps);
    // Any padding at the end of a frame stays zero.
    let mut out = vec![0u8; frames * stride];
    for (c, ch) in audio.iter().enumerate() {
        let slots = out
            .chunks_exact_mut(stride)
            .map(|f| &mut f[c * bps..(c + 1) * bps]);
        match fmt.format {
            SampleFormat::Int { bits: 8 } => {
                for (slot, &x) in slots.zip(ch) {
                    slot[0] = (encode_int(x, 8) + 128) as u8;
                }
            }
            SampleFormat::Int { bits } => {
                for (slot, &x) in slots.zip(ch) {
                    slot.copy_from_slice(&encode_int(x, bits).to_le_bytes()[..bps]);
                }
            }
            SampleFormat::Float { bits: 32 } => {
                for (slot, &x) in slots.zip(ch) {
                    slot.copy_from_slice(&x.to_le_bytes());
                }
            }
            SampleFormat::Float { .. } => {
                for (slot, &x) in slots.zip(ch) {
                    slot.copy_from_slice(&f64::from(x).to_le_bytes());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(channels: u16, format: SampleFormat) -> WavFormat {
        let bytes = match format {
            SampleFormat::Int { bits } | SampleFormat::Float { bits } => bits / 8,
        };
        WavFormat {
            channels,
            sample_rate: 44100,
            format,
            block_align: channels * bytes,
        }
    }

    /// The encoder as it was before it was sped up: per-sample `f64::round`, sample by sample.
    fn encode_reference(audio: &[Vec<f32>], fmt: &WavFormat) -> Vec<u8> {
        let frames = audio.iter().map(Vec::len).min().unwrap_or(0);
        let bps = bytes_per_sample(fmt.format);
        let pad = frame_stride(fmt).saturating_sub(audio.len() * bps);
        let int = |x: f32, bits: u16| {
            let scale = f64::from(1u32 << (bits - 1));
            (f64::from(x) * scale).round().clamp(-scale, scale - 1.0) as i64
        };
        let mut out = Vec::new();
        for i in 0..frames {
            for ch in audio {
                let x = ch[i];
                match fmt.format {
                    SampleFormat::Int { bits: 8 } => out.push((int(x, 8) + 128) as u8),
                    SampleFormat::Int { bits } => {
                        out.extend_from_slice(&int(x, bits).to_le_bytes()[..bps]);
                    }
                    SampleFormat::Float { bits: 32 } => out.extend_from_slice(&x.to_le_bytes()),
                    SampleFormat::Float { .. } => {
                        out.extend_from_slice(&f64::from(x).to_le_bytes());
                    }
                }
            }
            out.resize(out.len() + pad, 0);
        }
        out
    }

    #[test]
    fn encoding_matches_the_reference_byte_for_byte() {
        // Halfway values for every bit depth, clipping, specials and a sweep of ordinary values.
        let mut values = vec![
            0.0,
            -0.0,
            1.0,
            -1.0,
            1.5,
            -1.5,
            0.999_999_9,
            -0.999_999_9,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
        ];
        for bits in [8u16, 16, 24, 32] {
            let step = 1.0 / (1u64 << (bits - 1)) as f32;
            for k in [-3.5f32, -2.5, -1.5, -0.5, 0.5, 1.5, 2.5, 3.5] {
                values.push(k * step);
                values.push(1.0 + k * step);
                values.push(-1.0 - k * step);
            }
        }
        values.extend((0..20_000).map(|i| ((i as f32) * 0.618_034).sin() * 1.02));
        let audio = [values.clone(), values.iter().rev().copied().collect()];
        for format in [
            SampleFormat::Int { bits: 8 },
            SampleFormat::Int { bits: 16 },
            SampleFormat::Int { bits: 24 },
            SampleFormat::Int { bits: 32 },
            SampleFormat::Float { bits: 32 },
            SampleFormat::Float { bits: 64 },
        ] {
            for channels in [1u16, 2] {
                let f = fmt(channels, format);
                let a = &audio[..usize::from(channels)];
                assert!(
                    encode_frames(a, &f) == encode_reference(a, &f),
                    "{format:?} with {channels} channel(s)"
                );
            }
        }
        // A frame wider than its samples (an extensible layout) keeps zero padding.
        let mut padded = fmt(1, SampleFormat::Int { bits: 16 });
        padded.block_align = 4;
        assert_eq!(
            encode_frames(&audio[..1], &padded),
            encode_reference(&audio[..1], &padded)
        );
    }

    fn fmt_chunk(tag: u16, channels: u16, bits: u16) -> Vec<u8> {
        let block = channels * (bits / 8);
        let mut d = Vec::new();
        d.extend_from_slice(&tag.to_le_bytes());
        d.extend_from_slice(&channels.to_le_bytes());
        d.extend_from_slice(&44100u32.to_le_bytes());
        d.extend_from_slice(&(44100 * u32::from(block)).to_le_bytes());
        d.extend_from_slice(&block.to_le_bytes());
        d.extend_from_slice(&bits.to_le_bytes());
        d
    }

    #[test]
    fn decode_encode_roundtrip_int16_exact() {
        let f = fmt(1, SampleFormat::Int { bits: 16 });
        let bytes: Vec<u8> = (i16::MIN..=i16::MAX)
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let audio = decode(&bytes, &f);
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].len(), 65536);
        assert_eq!(encode_frames(&audio, &f), bytes);
    }

    #[test]
    fn decode_encode_roundtrip_int24_exact() {
        let f = fmt(1, SampleFormat::Int { bits: 24 });
        let mut state: u32 = 0x1234_5678;
        let mut bytes = Vec::new();
        for _ in 0..4096 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Take the high 24 bits: the low bits of an LCG are weak.
            bytes.extend_from_slice(&(state >> 8).to_le_bytes()[..3]);
        }
        let audio = decode(&bytes, &f);
        assert_eq!(audio[0].len(), 4096);
        assert_eq!(encode_frames(&audio, &f), bytes);
    }

    #[test]
    fn decode_uint8_offset() {
        let f = fmt(1, SampleFormat::Int { bits: 8 });
        let audio = decode(&[0x80, 0x00], &f);
        assert_eq!(audio, vec![vec![0.0, -1.0]]);
    }

    #[test]
    fn decode_float32_passthrough() {
        let f = fmt(1, SampleFormat::Float { bits: 32 });
        let audio = decode(&0.25f32.to_le_bytes(), &f);
        assert_eq!(audio, vec![vec![0.25]]);
    }

    #[test]
    fn encode_clips() {
        let f = fmt(1, SampleFormat::Int { bits: 16 });
        let bytes = encode_frames(&[vec![1.5, -1.5]], &f);
        let mut expected = 32767i16.to_le_bytes().to_vec();
        expected.extend_from_slice(&(-32768i16).to_le_bytes());
        assert_eq!(bytes, expected);
    }

    #[test]
    fn parse_fmt_extensible_pcm24() {
        let mut d = fmt_chunk(0xFFFE, 2, 24);
        d.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        d.extend_from_slice(&24u16.to_le_bytes()); // wValidBitsPerSample
        d.extend_from_slice(&3u32.to_le_bytes()); // channel mask
        d.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ]);
        let f = parse_fmt(&d).unwrap();
        assert_eq!(f.format, SampleFormat::Int { bits: 24 });
        assert_eq!(f.channels, 2);
        assert_eq!(f.sample_rate, 44100);
        assert_eq!(f.block_align, 6);
    }

    #[test]
    fn parse_fmt_rejects_adpcm() {
        let d = fmt_chunk(2, 1, 4);
        assert!(matches!(parse_fmt(&d), Err(WavError::UnsupportedFormat(_))));
    }

    #[test]
    fn parse_fmt_rejects_zero_channels_and_short_chunk() {
        assert!(matches!(
            parse_fmt(&fmt_chunk(1, 0, 16)),
            Err(WavError::Malformed(_))
        ));
        assert!(matches!(parse_fmt(&[0u8; 10]), Err(WavError::Malformed(_))));
    }

    #[test]
    fn decode_stereo_planar() {
        let f = fmt(2, SampleFormat::Int { bits: 16 });
        let (l0, r0, l1, r1) = (16384i16, -16384i16, 8192i16, -8192i16);
        let bytes: Vec<u8> = [l0, r0, l1, r1]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let audio = decode(&bytes, &f);
        assert_eq!(audio, vec![vec![0.5, 0.25], vec![-0.5, -0.25]]);
    }

    #[test]
    fn decode_drops_partial_trailing_frame() {
        let f = fmt(2, SampleFormat::Int { bits: 16 });
        let audio = decode(&[0u8; 6], &f);
        assert_eq!(audio, vec![vec![0.0], vec![0.0]]);
    }

    #[test]
    fn testwav_roundtrips_through_parsers() {
        use crate::riff::{find, parse_riff};
        let s = crate::testwav::sine(440.0, 44100, 100, 0.5);
        let bytes = crate::testwav::wav_bytes(&[s], 44100, SampleFormat::Int { bits: 16 }, vec![]);
        let riff = parse_riff(&bytes).unwrap();
        let f = parse_fmt(&riff.chunks[find(&riff.chunks, b"fmt ").unwrap()].data).unwrap();
        assert_eq!(f.format, SampleFormat::Int { bits: 16 });
        let d = &riff.chunks[find(&riff.chunks, b"data").unwrap()].data;
        assert_eq!(decode(d, &f)[0].len(), 100);
    }

    /// A 40-byte extensible `fmt ` chunk for PCM with the given channel mask.
    fn extensible_fmt(channels: u16, bits: u16, mask: u32) -> Vec<u8> {
        let mut d = fmt_chunk(0xFFFE, channels, bits);
        d.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        d.extend_from_slice(&bits.to_le_bytes()); // wValidBitsPerSample
        d.extend_from_slice(&mask.to_le_bytes());
        d.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ]);
        d
    }

    #[test]
    fn fmt_with_channels_plain_layout() {
        let original = fmt_chunk(1, 2, 16);
        let mono = fmt_with_channels(&original, 1);
        assert_eq!(mono, fmt_chunk(1, 1, 16));
        let f = parse_fmt(&mono).unwrap();
        assert_eq!((f.channels, f.block_align), (1, 2));
        assert_eq!(u32_at(&mono, 8), 44100 * 2);

        // Round trip, and a float format keeps its tag.
        assert_eq!(fmt_with_channels(&mono, 2), original);
        let float = fmt_with_channels(&fmt_chunk(3, 1, 32), 2);
        assert_eq!(float, fmt_chunk(3, 2, 32));
    }

    #[test]
    fn fmt_with_channels_extensible_layout() {
        let stereo = extensible_fmt(2, 24, 3);
        let mono = fmt_with_channels(&stereo, 1);
        assert_eq!(mono, extensible_fmt(1, 24, 4));
        let f = parse_fmt(&mono).unwrap();
        assert_eq!((f.channels, f.block_align), (1, 3));
        assert_eq!(u32_at(&mono, 8), 44100 * 3);
        assert_eq!(u32_at(&mono, 20), 0x4);

        assert_eq!(fmt_with_channels(&mono, 2), stereo);
        // The sub-format GUID and everything after the mask are untouched.
        assert_eq!(mono[24..], stereo[24..]);
    }

    #[test]
    fn fmt_with_channels_leaves_a_short_chunk_alone() {
        assert_eq!(fmt_with_channels(&[1, 0, 2], 1), vec![1, 0, 2]);
    }

    #[test]
    fn fmt_with_rate_sets_rate_and_byte_rate() {
        // Plain PCM: the rate and the byte rate change, the rest stays.
        let plain = fmt_with_rate(&fmt_chunk(1, 2, 16), 31_250);
        let f = parse_fmt(&plain).unwrap();
        assert_eq!((f.sample_rate, f.channels, f.block_align), (31_250, 2, 4));
        assert_eq!(u32_at(&plain, 8), 31_250 * 4);
        assert_eq!(plain[12..], fmt_chunk(1, 2, 16)[12..]);

        // Float keeps its tag.
        let float = fmt_with_rate(&fmt_chunk(3, 1, 32), 96_000);
        assert_eq!(float[..4], fmt_chunk(3, 1, 32)[..4]);
        assert_eq!(parse_fmt(&float).unwrap().sample_rate, 96_000);
        assert_eq!(u32_at(&float, 8), 96_000 * 4);

        // Extensible: everything after the byte rate is untouched, including the mask.
        let ext = extensible_fmt(2, 24, 3);
        let out = fmt_with_rate(&ext, 22_050);
        assert_eq!(parse_fmt(&out).unwrap().sample_rate, 22_050);
        assert_eq!(u32_at(&out, 8), 22_050 * 6);
        assert_eq!(out[12..], ext[12..]);
    }

    #[test]
    fn fmt_with_rate_saturates_and_leaves_a_short_chunk_alone() {
        let mut wide = fmt_chunk(1, 2, 16);
        wide[12..14].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(u32_at(&fmt_with_rate(&wide, u32::MAX), 8), u32::MAX);
        assert_eq!(fmt_with_rate(&[1, 0, 2], 8_000), vec![1, 0, 2]);
    }
}
