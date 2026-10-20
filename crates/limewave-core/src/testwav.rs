//! Synthetic WAV builders for tests.

use crate::riff::{Chunk, RiffFile, write_riff};
use crate::wav::{SampleFormat, WavFormat, encode_frames};

/// A sine wave of `frames` samples at `freq` Hz.
pub fn sine(freq: f32, rate: u32, frames: usize, amp: f32) -> Vec<f32> {
    let step = std::f64::consts::TAU * f64::from(freq) / f64::from(rate);
    (0..frames)
        .map(|n| amp * (step * n as f64).sin() as f32)
        .collect()
}

/// A complete RIFF/WAVE file: a plain `fmt ` chunk, then `extra`, then `data`.
pub fn wav_bytes(
    channels: &[Vec<f32>],
    rate: u32,
    format: SampleFormat,
    extra: Vec<Chunk>,
) -> Vec<u8> {
    let (tag, bits) = match format {
        SampleFormat::Int { bits } => (1u16, bits),
        SampleFormat::Float { bits } => (3u16, bits),
    };
    let n_channels = u16::try_from(channels.len()).expect("too many channels");
    let block_align = n_channels * (bits / 8);
    let fmt = WavFormat {
        channels: n_channels,
        sample_rate: rate,
        format,
        block_align,
    };
    let mut fmt_data = Vec::with_capacity(16);
    fmt_data.extend_from_slice(&tag.to_le_bytes());
    fmt_data.extend_from_slice(&n_channels.to_le_bytes());
    fmt_data.extend_from_slice(&rate.to_le_bytes());
    fmt_data.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
    fmt_data.extend_from_slice(&block_align.to_le_bytes());
    fmt_data.extend_from_slice(&bits.to_le_bytes());

    let mut chunks = vec![Chunk {
        id: *b"fmt ",
        data: fmt_data,
    }];
    chunks.extend(extra);
    chunks.push(Chunk {
        id: *b"data",
        data: encode_frames(channels, &fmt),
    });
    write_riff(&RiffFile {
        chunks,
        warnings: Vec::new(),
    })
}
