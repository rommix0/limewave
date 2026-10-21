//! The whole v0.3 flow on a real file: open a 16-bit WAV with cues and a loop, move End
//! earlier and generate a Sustain tail, make a spectrum loop from a selection before the cut,
//! save, reload, and save again.

use std::path::PathBuf;

use limewave_core::cue::{CuePoint, cue_bytes, parse_cue};
use limewave_core::document::load_wav_bytes;
use limewave_core::effects::{Op, process};
use limewave_core::render::render;
use limewave_core::riff::{Chunk, find};
use limewave_core::save::build_wav_bytes;
use limewave_core::smpl::{Smpl, SmplLoop};
use limewave_core::spectrum_loop::{SpectrumLoopSettings, render_spectrum_loop};
use limewave_core::tail::{TailMethod, TailMode, TailSettings, render_tail};
use limewave_core::testwav::wav_bytes;
use limewave_core::wav::SampleFormat;

const RATE: u32 = 44_100;
/// 1.5 s.
const FRAMES: usize = 66_150;
/// The first frame after the new End.
const CUT: usize = 52_000;
/// The spectrum loop's selection.
const RANGE: (usize, usize) = (26_000, 39_999);
/// The spectrum loop's transitions (100 ms) and loop (0.5 s), in frames.
const T: usize = 4_410;
const L: usize = 22_050;

/// A steady 220 Hz tone with a few harmonics; each channel a little different.
fn tone(channel: usize) -> Vec<f32> {
    (0..FRAMES)
        .map(|i| {
            let t = i as f64 / f64::from(RATE);
            let x: f64 = (1..=4)
                .map(|h| {
                    let w = std::f64::consts::TAU * 220.0 * h as f64 * t;
                    (w + channel as f64).sin() / h as f64
                })
                .sum();
            (0.3 * x) as f32
        })
        .collect()
}

fn cue(id: u32, position: u32) -> CuePoint {
    CuePoint {
        id,
        position,
        chunk_id: *b"data",
        chunk_start: 0,
        block_start: 0,
        sample_offset: position,
    }
}

/// Loop 0 on 10000..=20000, and five cues: before everything (1), inside the spectrum range
/// (2), between the range and the cut (3), after the cut (4) and at the end of the file (5).
fn chunks() -> Vec<Chunk> {
    let smpl = Smpl {
        manufacturer: 0,
        product: 0,
        sample_period: 22_675,
        unity_note: 60,
        pitch_fraction: 0,
        smpte_format: 0,
        smpte_offset: 0,
        loops: vec![SmplLoop {
            cue_id: 0,
            loop_type: 0,
            start: 10_000,
            end: 20_000,
            fraction: 0,
            play_count: 0,
        }],
        sampler_data: Vec::new(),
    };
    let cues = [
        cue(1, 5_000),
        cue(2, 30_000),
        cue(3, 46_000),
        cue(4, 60_000),
        cue(5, FRAMES as u32),
    ];
    vec![
        Chunk {
            id: *b"smpl",
            data: smpl.to_bytes(),
        },
        Chunk {
            id: *b"cue ",
            data: cue_bytes(&cues),
        },
    ]
}

fn flow(channels: usize, method: TailMethod) {
    let name = format!("{channels} ch, {method:?}");
    let audio: Vec<Vec<f32>> = (0..channels).map(tone).collect();
    let bytes = wav_bytes(&audio, RATE, SampleFormat::Int { bits: 16 }, chunks());
    let doc = load_wav_bytes(PathBuf::from("flow.wav"), &bytes).unwrap();
    assert_eq!(doc.frames, FRAMES);

    // End earlier, then a Sustain tail from the cut.
    let mut e = doc.initial_edit();
    assert_eq!((e.loop_start, e.loop_end), (10_000, 20_000), "{name}");
    e.end = CUT - 1;
    let tail = TailSettings {
        mode: TailMode::Sustain,
        method,
        body_s: 0.5,
        release_t60: Some(0.2),
        ..TailSettings::default()
    };
    let processed = process(&doc.audio, &e.effects);
    let r = render_tail(&processed, &e, None, RATE, &tail).expect("the tail renders");
    let e = r.edit;
    assert_eq!(e.loop_start, CUT, "{name}: loop 0 on the body");

    // A spectrum loop from a selection before the cut.
    let spectrum = SpectrumLoopSettings {
        loop_s: 0.5,
        transition_ms: 100.0,
        ..SpectrumLoopSettings::default()
    };
    let processed = process(&doc.audio, &e.effects);
    let r = render_spectrum_loop(&processed, &e, Some(RANGE), RATE, &spectrum)
        .expect("the spectrum loop renders");
    let (a, b) = (RANGE.0, RANGE.1 + 1);
    let Op::Splice(sp) = &r.step.op else {
        panic!("not a splice")
    };
    assert_eq!(
        (r.step.first, r.step.last, sp.audio[0].len()),
        (a, b + T - 1, T + L + T),
        "{name}: the step replaces a..=b+T-1 with the transitions and the loop"
    );
    let e = r.edit;
    assert_eq!(
        e.loop_end + 1 - e.loop_start,
        L,
        "{name}: loop 0 on the loop"
    );
    let want = render(&process(&doc.audio, &e.effects), &e);
    let frames = e.end + 1 - e.start;
    assert_eq!(want[0].len(), frames, "{name}: End is the last frame");

    // Save and reload.
    let saved = build_wav_bytes(&doc, &e);
    let back = load_wav_bytes(PathBuf::from("flow.wav"), &saved).unwrap();
    assert_eq!(back.frames, frames, "{name}: frame count");
    assert_eq!(back.audio.len(), channels);
    let smpl = back.smpl.as_ref().expect("smpl");
    let l0 = &smpl.loops[0];
    assert_eq!(
        (l0.loop_type, l0.start as usize, l0.end as usize),
        (0, e.loop_start - e.start, e.loop_end - e.start),
        "{name}: loop 0"
    );
    let lsb = 1.0 / 32_768.0;
    for (c, (got, want)) in back.audio.iter().zip(&want).enumerate() {
        let worst = got
            .iter()
            .zip(&want[e.start..=e.end])
            .map(|(g, w)| (g - w).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst <= lsb * 1.001,
            "{name}: channel {c} differs by {worst}"
        );
    }

    // The cues: kept before the range, dropped inside it and after the cut, shifted after
    // the range, and the end-of-file cue on the new end.
    let at = find(&back.riff.chunks, b"cue ").expect("cue chunk");
    let cues = parse_cue(&back.riff.chunks[at].data).unwrap();
    let position = |id: u32| {
        cues.iter()
            .find(|p| p.id == id)
            .map(|p| p.position as usize)
    };
    let shift = T + L - (b - a);
    assert_eq!(position(1), Some(5_000), "{name}: before the range");
    assert_eq!(position(2), None, "{name}: inside the spectrum range");
    assert_eq!(position(3), Some(46_000 + shift), "{name}: after the range");
    assert_eq!(position(4), None, "{name}: after the cut");
    assert_eq!(position(5), Some(frames), "{name}: end of file");
    assert_eq!(cues.len(), 3, "{name}");

    // Saving the reloaded file again gives it back unchanged.
    let again = build_wav_bytes(&back, &back.initial_edit());
    assert!(again == saved, "{name}: a second save is stable");
}

#[test]
fn tail_spectrum_loop_and_save_flow_mono() {
    flow(1, TailMethod::Granular);
}

#[test]
fn tail_spectrum_loop_and_save_flow_stereo() {
    flow(2, TailMethod::Smooth);
}
