//! Writes a tiny pack of test WAVs for checking tuning and looping in a sampler.
//!
//! ```text
//! cargo run -p seamlime-core --example check_pack --features testwav -- <out dir>
//! ```
//!
//! Each file is 48 kHz, 16-bit mono and about two seconds long, with MIDI root note 69 (A4):
//!
//! - `sine_A4_plus25c.wav`: a sine 25 cents above 440 Hz, tuned +25 cents.
//! - `sine_A4_minus25c.wav`: a sine 25 cents below 440 Hz, tuned -25 cents.
//! - `sine_A4_loop.wav`: a 440 Hz sine with a forward loop of exactly 110 periods.
//!
//! Loaded into a sampler, each should sound in tune at A4, and the third should loop cleanly.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use seamlime_core::document::load_wav;
use seamlime_core::riff::Chunk;
use seamlime_core::smpl::{Smpl, SmplLoop, sample_period, tuning_to_smpl};
use seamlime_core::testwav::{sine, wav_bytes};
use seamlime_core::wav::SampleFormat;

const RATE: u32 = 48_000;
const FRAMES: usize = 96_000;
const ROOT: u8 = 69;
const A4_HZ: f64 = 440.0;
/// 48 000 / 440 is not a whole number of frames, but 1 200 frames is exactly 11 periods.
const LOOP_STEP: usize = 1_200;
const LOOP_PERIODS_OF_STEP: usize = 10;
const LOOP_START: usize = 24_000;

fn smpl_chunk(cents: f32, loop_range: Option<(usize, usize)>) -> Chunk {
    let (unity_note, pitch_fraction) = tuning_to_smpl(ROOT, cents);
    let loops = loop_range
        .map(|(start, end)| {
            vec![SmplLoop {
                cue_id: 0,
                loop_type: 0,
                start: start as u32,
                // The loop end is inclusive.
                end: end as u32,
                fraction: 0,
                play_count: 0,
            }]
        })
        .unwrap_or_default();
    let smpl = Smpl {
        manufacturer: 0,
        product: 0,
        sample_period: sample_period(RATE),
        unity_note,
        pitch_fraction,
        smpte_format: 0,
        smpte_offset: 0,
        loops,
        sampler_data: Vec::new(),
    };
    Chunk {
        id: *b"smpl",
        data: smpl.to_bytes(),
    }
}

fn write_sine(dir: &Path, name: &str, freq: f64, smpl: Chunk) -> std::io::Result<PathBuf> {
    let audio = vec![sine(freq as f32, RATE, FRAMES, 0.5)];
    let bytes = wav_bytes(&audio, RATE, SampleFormat::Int { bits: 16 }, vec![smpl]);
    let path = dir.join(name);
    std::fs::write(&path, bytes)?;
    Ok(path)
}

fn main() -> ExitCode {
    let Some(dir) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: check_pack <out dir>");
        return ExitCode::FAILURE;
    };
    match run(&dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("check_pack: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let loop_len = LOOP_STEP * LOOP_PERIODS_OF_STEP;
    let loop_range = (LOOP_START, LOOP_START + loop_len - 1);
    let files = [
        (
            "sine_A4_plus25c.wav",
            A4_HZ * (25.0f64 / 1200.0).exp2(),
            25.0,
            None,
        ),
        (
            "sine_A4_minus25c.wav",
            A4_HZ * (-25.0f64 / 1200.0).exp2(),
            -25.0,
            None,
        ),
        ("sine_A4_loop.wav", A4_HZ, 0.0, Some(loop_range)),
    ];
    for (name, freq, cents, looped) in files {
        let path = write_sine(dir, name, freq, smpl_chunk(cents, looped))?;
        // Read the file back through the library to make sure it says what we meant.
        let doc = load_wav(&path)?;
        let edit = doc.initial_edit();
        assert_eq!(edit.root, ROOT, "{name}: root note");
        assert!((edit.cents - cents).abs() < 0.001, "{name}: fine tune");
        println!("wrote {} ({freq:.4} Hz)", path.display());
    }
    Ok(())
}
