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
    let mut out = vec![Vec::with_capacity(data.len() / stride); channels];
    for frame in data.chunks_exact(stride) {
        for (ch, samples) in out.iter_mut().enumerate() {
            samples.push(decode_sample(&frame[ch * bps..(ch + 1) * bps], fmt.format));
        }
    }
    out
}

/// Scales to `2^(bits-1)`, rounds to nearest and clips to the signed range.
fn encode_int(x: f32, bits: u16) -> i64 {
    let scale = f64::from(1u32 << (bits - 1));
    (f64::from(x) * scale).round().clamp(-scale, scale - 1.0) as i64
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
    let pad = frame_stride(fmt).saturating_sub(audio.len() * bps);
    let mut out = Vec::with_capacity(frames * (audio.len() * bps + pad));
    for i in 0..frames {
        for ch in audio {
            let x = ch[i];
            match fmt.format {
                SampleFormat::Int { bits: 8 } => out.push((encode_int(x, 8) + 128) as u8),
                SampleFormat::Int { bits } => {
                    out.extend_from_slice(&encode_int(x, bits).to_le_bytes()[..bps]);
                }
                SampleFormat::Float { bits: 32 } => out.extend_from_slice(&x.to_le_bytes()),
                SampleFormat::Float { .. } => out.extend_from_slice(&f64::from(x).to_le_bytes()),
            }
        }
        out.resize(out.len() + pad, 0);
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
}
