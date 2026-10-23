//! Spectrogram analysis on a musical frequency axis, quantised to bytes for drawing.
//!
//! Each column is one Hann-windowed FFT centred on a stretch of `hop` frames. The `ROWS` rows
//! are spaced evenly in octaves from [`LOWEST_HZ`] (the note C1) up to the Nyquist frequency,
//! with row 0 at the top. Every cell is a level in dBFS mapped linearly onto `0..=255`, with
//! [`FLOOR_DB`] and below as 0 and full scale as 255. The result is plain bytes so a UI can
//! upload it as a texture without any further maths.

use crate::dsp::spectrum::{Fft, hann};

/// The lowest frequency shown: C1, at the bottom edge of the axis.
pub const LOWEST_HZ: f64 = 32.703_195_662_574_83;

/// The level that maps to byte 0; anything quieter is clamped to it.
pub const FLOOR_DB: f32 = -90.0;

/// The number of frequency rows in a spectrogram.
pub const ROWS: usize = 512;

/// The most columns a spectrogram gets, however long the audio is.
pub const MAX_COLUMNS: usize = 16_384;

/// A byte-quantised spectrogram of every channel of some audio.
#[derive(Clone, Debug, PartialEq)]
pub struct Spectrogram {
    /// The number of columns (time steps): `frames.div_ceil(hop)`.
    pub columns: usize,
    /// Frames of audio covered by one column; column `c` spans `c·hop .. (c+1)·hop`.
    pub hop: usize,
    /// The sample rate the audio was analysed at.
    pub rate: u32,
    /// The length of the analysed audio, in frames.
    pub frames: usize,
    /// One image per channel, `ROWS × columns` bytes in row-major order
    /// (`row * columns + column`). Row 0 is at the top and holds the Nyquist frequency.
    /// A value is 0 at [`FLOOR_DB`] or below and 255 at 0 dBFS or above, linear in dB between.
    pub channels: Vec<Vec<u8>>,
}

/// Frames per column for audio of `frames` frames: just enough to stay within [`MAX_COLUMNS`],
/// and never fewer than 64.
pub fn hop_for(frames: usize) -> usize {
    frames.div_ceil(MAX_COLUMNS).max(64)
}

/// The FFT size for sample rate `rate`: the largest power of two that is at most `rate / 20`
/// (a window of about 50 ms), kept within 256 to 16,384.
pub fn fft_size_for(rate: u32) -> usize {
    let target = (rate / 20) as usize;
    let n = if target == 0 { 1 } else { 1 << target.ilog2() };
    n.clamp(256, 16_384)
}

/// The Nyquist frequency of `rate`, kept just above [`LOWEST_HZ`] so the axis never collapses.
fn nyquist(rate: u32) -> f64 {
    (f64::from(rate) / 2.0).max(LOWEST_HZ * 1.5)
}

/// The frequency at the centre of row `row` for sample rate `rate`. Row 0 is just under the
/// Nyquist frequency and row `ROWS − 1` just over [`LOWEST_HZ`].
pub fn row_hz(row: usize, rate: u32) -> f64 {
    let t = 1.0 - (row as f64 + 0.5) / ROWS as f64;
    LOWEST_HZ * (nyquist(rate) / LOWEST_HZ).powf(t)
}

/// How far down the axis `hz` sits: 0 at the top (the Nyquist frequency) to 1 at the bottom
/// ([`LOWEST_HZ`]), clamped to that range.
pub fn hz_y_fraction(hz: f64, rate: u32) -> f32 {
    let f = 1.0 - (hz / LOWEST_HZ).ln() / (nyquist(rate) / LOWEST_HZ).ln();
    // A nonsense (NaN) frequency lands on the top edge.
    if f.is_nan() {
        0.0
    } else {
        f.clamp(0.0, 1.0) as f32
    }
}

/// Where one row reads its level from in an FFT's magnitude bins.
struct RowSource {
    /// The first bin to read.
    first: usize,
    /// The last bin to read, inclusive. Equal to `first` when `frac` is used.
    last: usize,
    /// When the row is narrower than a bin: the share of bin `first + 1` blended in with
    /// bin `first`. Negative when the row takes the largest bin in `first..=last` instead.
    frac: f32,
}

/// The bin range of every row for an FFT of `n` points at `rate`. Each row spans from the
/// geometric midpoint with the row above to the one with the row below.
fn row_sources(n: usize, rate: u32) -> Vec<RowSource> {
    let bins = n / 2 + 1;
    let bin_hz = f64::from(rate) / n as f64;
    let step = (nyquist(rate) / LOWEST_HZ).powf(1.0 / ROWS as f64);
    let half_step = step.sqrt();
    (0..ROWS)
        .map(|row| {
            let centre = row_hz(row, rate);
            let lo = centre / half_step / bin_hz;
            let hi = centre * half_step / bin_hz;
            if hi - lo < 1.0 {
                let pos = centre / bin_hz;
                let first = (pos.floor() as usize).min(bins - 2);
                RowSource {
                    first,
                    last: first,
                    frac: (pos - first as f64).clamp(0.0, 1.0) as f32,
                }
            } else {
                // At least one bin centre lies inside a band one bin or wider.
                let first = (lo.ceil() as usize).min(bins - 1);
                let last = (hi.floor() as usize).clamp(first, bins - 1);
                RowSource {
                    first,
                    last,
                    frac: -1.0,
                }
            }
        })
        .collect()
}

/// Analyses every channel of `audio` (all the same length) at `rate`.
///
/// The Hann window of [`fft_size_for`] points is centred on the middle of each column's
/// `hop` frames and treats anything outside the audio as silence. A bin's level is
/// `20·log10(|X| · 4 / n)`, so a full-scale sine reads 0 dBFS. A row covers the band from the
/// geometric midpoint with the row above to the one with the row below, so the bands tile the
/// axis. It takes the loudest bin in that band, or blends the two nearest bins when the band
/// is narrower than one bin.
pub fn spectrogram(audio: &[Vec<f32>], rate: u32) -> Spectrogram {
    let frames = audio.iter().map(Vec::len).max().unwrap_or(0);
    let hop = hop_for(frames);
    let columns = frames.div_ceil(hop);
    let n = fft_size_for(rate);
    let fft = Fft::new(n);
    let window = hann(n);
    let sources = row_sources(n, rate);
    let scale = 4.0 / n as f32;
    let byte_per_db = 255.0 / -FLOOR_DB;

    let mut windowed = vec![0.0f32; n];
    let mut mags = vec![0.0f32; n / 2 + 1];
    let channels = audio
        .iter()
        .map(|ch| {
            let mut image = vec![0u8; ROWS * columns];
            for c in 0..columns {
                // The window's peak, at index n / 2, sits on the middle of the column.
                let start = (c * hop + hop / 2) as isize - (n / 2) as isize;
                for (i, (out, w)) in windowed.iter_mut().zip(&window).enumerate() {
                    let sample = usize::try_from(start + i as isize)
                        .ok()
                        .and_then(|p| ch.get(p))
                        .copied()
                        .unwrap_or(0.0);
                    *out = sample * w;
                }
                for (m, bin) in mags.iter_mut().zip(fft.forward(&windowed)) {
                    *m = bin.norm() * scale;
                }
                for (row, src) in sources.iter().enumerate() {
                    let mag = if src.frac >= 0.0 {
                        mags[src.first] * (1.0 - src.frac) + mags[src.first + 1] * src.frac
                    } else {
                        mags[src.first..=src.last]
                            .iter()
                            .fold(0.0f32, |a, &b| a.max(b))
                    };
                    let db = 20.0 * mag.max(1e-12).log10();
                    image[row * columns + c] =
                        ((db - FLOOR_DB) * byte_per_db).round().clamp(0.0, 255.0) as u8;
                }
            }
            image
        })
        .collect();

    Spectrogram {
        columns,
        hop,
        rate,
        frames,
        channels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn sine(hz: f32, amp: f32, frames: usize, rate: u32) -> Vec<f32> {
        (0..frames)
            .map(|i| amp * (TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    /// The row with the largest value in column `c`, the topmost on a tie.
    fn brightest_row(s: &Spectrogram, ch: usize, c: usize) -> usize {
        let mut best = 0;
        for row in 1..ROWS {
            if s.channels[ch][row * s.columns + c] > s.channels[ch][best * s.columns + c] {
                best = row;
            }
        }
        best
    }

    #[test]
    fn a_440_hz_sine_is_brightest_on_a4() {
        let s = spectrogram(&[sine(440.0, 0.5, 44_100, 44_100)], 44_100);
        let row = brightest_row(&s, 0, s.columns / 2);
        let semitones = 12.0 * (row_hz(row, 44_100) / 440.0).log2();
        assert!(
            semitones.abs() < 1.0,
            "row {row} is {semitones} semitones off"
        );
    }

    #[test]
    fn the_axis_runs_from_c1_to_the_nyquist() {
        let r = 22_050;
        let step = (11_025.0f64 / LOWEST_HZ).powf(1.0 / ROWS as f64);
        let top = row_hz(0, r);
        assert!(
            top / 11_025.0 < step && 11_025.0 / top < step,
            "top is {top}"
        );
        let bottom = row_hz(ROWS - 1, r);
        assert!(bottom / LOWEST_HZ < step, "bottom is {bottom}");
        assert_eq!(hz_y_fraction(LOWEST_HZ, r), 1.0);
        assert_eq!(hz_y_fraction(f64::from(r) / 2.0, r), 0.0);
        assert_eq!(hz_y_fraction(1.0, r), 1.0);
        assert_eq!(hz_y_fraction(1.0e9, r), 0.0);
    }

    #[test]
    fn silence_is_floor_and_a_full_scale_sine_is_bright() {
        let quiet = spectrogram(&[vec![0.0; 20_000]], 44_100);
        assert!(quiet.channels[0].iter().all(|&v| v == 0));
        let loud = spectrogram(&[sine(1000.0, 1.0, 44_100, 44_100)], 44_100);
        let peak = loud.channels[0].iter().copied().max().unwrap();
        assert!(peak >= 240, "peak is {peak}");
        // Half the amplitude is 6.02 dB down: 255 · (1 − 6.02 / 90) ≈ 238. The window's
        // scalloping loss between bins is at most 1.42 dB (4 bytes), so allow that much below.
        let half = spectrogram(&[sine(1000.0, 0.5, 44_100, 44_100)], 44_100);
        let peak = half.channels[0].iter().copied().max().unwrap();
        assert!((233..=239).contains(&peak), "peak is {peak}");
    }

    #[test]
    fn a_column_is_centred_on_the_middle_of_its_hop() {
        // The 2048-point window is far wider than a 64-frame hop, so neighbouring columns
        // barely differ near the impulse. Further out the window's slope is steep: columns
        // the same distance either side of the centre only match if it is exactly centred.
        let mut x = vec![0.0f32; 20_000];
        let hop = hop_for(x.len());
        assert_eq!(hop, 64);
        let c = 100;
        x[c * hop + hop / 2] = 1.0;
        let s = spectrogram(&[x], 44_100);
        let level = |col: usize| (0..ROWS).map(|r| s.channels[0][r * s.columns + col]).max();
        let at_c = level(c);
        assert!((0..s.columns).all(|col| level(col) <= at_c));
        // Twelve columns out the window is down by about 17 dB, and a half-hop shift would
        // make one side about 7 bytes louder than the other.
        let (before, after) = (level(c - 12).unwrap(), level(c + 12).unwrap());
        assert!(
            before > 30 && before.abs_diff(after) <= 1,
            "{before} vs {after}"
        );
    }

    #[test]
    fn columns_follow_the_hop() {
        let s = spectrogram(&[vec![0.0; 44_100]], 44_100);
        assert_eq!(s.hop, 64);
        assert_eq!(s.columns, 690);
        assert_eq!(s.frames, 44_100);
        assert_eq!(s.rate, 44_100);
        assert_eq!(s.channels[0].len(), ROWS * 690);
        let ten_minutes = 26_460_000;
        let hop = hop_for(ten_minutes);
        assert!(ten_minutes.div_ceil(hop) <= MAX_COLUMNS);
        assert_eq!(fft_size_for(44_100), 2048);
        assert_eq!(fft_size_for(96_000), 4096);
        assert_eq!(fft_size_for(0), 256);
        assert_eq!(fft_size_for(u32::MAX), 16_384);
    }

    #[test]
    fn tiny_files_do_not_panic() {
        let s = spectrogram(&[vec![0.5; 10]], 44_100);
        assert_eq!(s.columns, 1);
        assert_eq!(s.channels[0].len(), ROWS);
        let empty = spectrogram(&[Vec::new()], 44_100);
        assert_eq!(empty.columns, 0);
        assert!(empty.channels[0].is_empty());
        assert_eq!(spectrogram(&[], 44_100).columns, 0);
    }

    #[test]
    fn stereo_channels_are_analysed_separately() {
        let s = spectrogram(
            &[vec![0.0; 22_050], sine(1000.0, 0.8, 22_050, 44_100)],
            44_100,
        );
        assert_eq!(s.channels.len(), 2);
        assert!(s.channels[0].iter().all(|&v| v == 0));
        assert!(s.channels[1].iter().any(|&v| v > 200));
    }
}
