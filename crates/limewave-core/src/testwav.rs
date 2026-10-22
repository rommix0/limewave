//! Synthetic WAV builders for tests.

use crate::riff::{Chunk, RiffFile, write_riff};
use crate::wav::{SampleFormat, WavFormat, encode_frames, fmt_bytes};

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
    let (SampleFormat::Int { bits } | SampleFormat::Float { bits }) = format;
    let n_channels = u16::try_from(channels.len()).expect("too many channels");
    let block_align = n_channels * (bits / 8);
    let fmt = WavFormat {
        channels: n_channels,
        sample_rate: rate,
        format,
        block_align,
    };
    let mut chunks = vec![Chunk {
        id: *b"fmt ",
        data: fmt_bytes(&fmt),
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
