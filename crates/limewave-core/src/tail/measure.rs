//! Measurements of a rendered tail, as v0.3 §8.1 defines them: the join and loop-wrap steps,
//! the level at the join, the steadiness of a sustain body, the envelope against a real ending
//! and the stereo correlation. The tail tests use them, and so does the `tail_lab` bench.
//!
//! Ranges are inclusive (`first..=last`) and clamped to the shortest channel. Channels are
//! combined by energy or by their largest step, never by summing samples.
//!
//! The references a measure compares against (the source region's steps, the level at the
//! cut) always come from the *input*, the audio before the tail step: the rendered audio
//! already has its join overlap replaced, so a faulty join could inflate its own reference.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use super::{TailMethod, TailMode, TailSettings, render_tail};
use crate::document::{WavDocument, load_wav};
use crate::dsp::envelope::{level_db, rms_db_frames};
use crate::edit::LoopMode;
use crate::effects::process;
use crate::render::render;
use crate::riff::{Chunk, RiffFile, find, write_riff};
use crate::save::build_wav_bytes;
use crate::wav::encode_frames;

/// Steps within this many seconds either side of a join are checked.
const JOIN_WINDOW_S: f64 = 0.002;

/// The level after the cut is measured over this many seconds.
const LEVEL_WINDOW_S: f64 = 0.050;

/// A sustain body's level is measured in frames of this many seconds.
const BODY_FRAME_S: f64 = 0.050;

/// The envelope comparison uses frames of this many seconds...
const ENVELOPE_FRAME_S: f64 = 0.010;

/// ...until both envelopes are below this level.
const ENVELOPE_FLOOR_DB: f32 = -60.0;

/// Keeps a ratio finite when its reference is silent.
const TINY: f32 = 1e-12;

/// The shortest channel's length.
fn frames(audio: &[Vec<f32>]) -> usize {
    audio.iter().map(Vec::len).min().unwrap_or(0)
}

/// Frames in `secs` seconds at `rate`, at least 1.
fn frames_in(secs: f64, rate: u32) -> usize {
    ((secs * f64::from(rate)) as usize).max(1)
}

/// The step from frame `i` to frame `i + 1`: the largest absolute difference over channels.
fn step(audio: &[Vec<f32>], i: usize, j: usize) -> f32 {
    audio
        .iter()
        .map(|ch| (ch[j] - ch[i]).abs())
        .fold(0.0, f32::max)
}

/// The 99th percentile (linearly interpolated) of the sample-to-sample steps inside
/// `first..=last`, each the largest over channels. 0 when the range holds no step.
pub fn step_percentile_99(audio: &[Vec<f32>], first: usize, last: usize) -> f32 {
    let last = last.min(frames(audio).saturating_sub(1));
    if first >= last {
        return 0.0;
    }
    let mut steps: Vec<f32> = (first..last).map(|i| step(audio, i, i + 1)).collect();
    steps.sort_unstable_by(f32::total_cmp);
    let pos = 0.99 * (steps.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(steps.len() - 1);
    let frac = (pos - lo as f64) as f32;
    steps[lo] + (steps[hi] - steps[lo]) * frac
}

/// The join score at frame `at` of `audio`: the largest step within 2 ms of `at` (from
/// `at − 2 ms` to `at + 2 ms`), divided by the 99th-percentile step inside the source region
/// of `input`, the audio before the tail. 1.5 or less counts as click-free (v0.3 §8.1).
pub fn join_score(
    audio: &[Vec<f32>],
    at: usize,
    input: &[Vec<f32>],
    source: (usize, usize),
    rate: u32,
) -> f32 {
    let len = frames(audio);
    if len < 2 {
        return 0.0;
    }
    let w = frames_in(JOIN_WINDOW_S, rate);
    let lo = at.saturating_sub(w).min(len - 2);
    let hi = at.saturating_add(w).min(len - 1);
    let worst = (lo..hi).map(|i| step(audio, i, i + 1)).fold(0.0, f32::max);
    worst / (step_percentile_99(input, source.0, source.1) + TINY)
}

/// The loop-wrap score of the inclusive loop `(start, end)` of `audio`: the largest step in
/// the 2 ms before the loop end followed by the 2 ms from the loop start, as a wrap plays them,
/// divided by the 99th-percentile step inside the source region of `input`.
pub fn wrap_score(
    audio: &[Vec<f32>],
    (start, end): (usize, usize),
    input: &[Vec<f32>],
    source: (usize, usize),
    rate: u32,
) -> f32 {
    let len = frames(audio);
    if end >= len || start > end {
        return 0.0;
    }
    let w = frames_in(JOIN_WINDOW_S, rate);
    // The frames a wrap plays: end − w + 1 ..= end, then start .. start + w.
    let before = end.saturating_sub(w - 1).max(start);
    let order: Vec<usize> = (before..=end)
        .chain(start..(start + w).min(end + 1))
        .collect();
    let worst = order
        .windows(2)
        .map(|p| step(audio, p[0], p[1]))
        .fold(0.0, f32::max);
    worst / (step_percentile_99(input, source.0, source.1) + TINY)
}

/// The level of the first 50 ms of `audio` from `cut` (the first new frame), compared with
/// `level_at_cut_db` (the analysis of the input measures it, [`super::TailAnalysis`]), allowing
/// for a tail that falls at `rate_db_s` dB per second over those 50 ms. Positive is quieter
/// than it should be, in dB.
pub fn level_step_db(
    audio: &[Vec<f32>],
    cut: usize,
    level_at_cut_db: f32,
    rate: u32,
    rate_db_s: f32,
) -> f32 {
    let n = frames_in(LEVEL_WINDOW_S, rate);
    let after = level_db(audio, cut, cut + n - 1);
    // The mean energy of a decay over the window, relative to its start.
    let per_frame = f64::from(rate_db_s) / f64::from(rate.max(1));
    let mean: f64 = (0..n)
        .map(|i| 10f64.powf(-per_frame * i as f64 / 10.0))
        .sum::<f64>()
        / n as f64;
    let expected = (10.0 * mean.log10()) as f32;
    level_at_cut_db + expected - after
}

/// The 50 ms RMS levels of `first..=last` in dB, frame by frame from `first`.
pub fn body_levels_db(audio: &[Vec<f32>], first: usize, last: usize, rate: u32) -> Vec<f32> {
    rms_db_frames(audio, first, last, frames_in(BODY_FRAME_S, rate))
}

/// How far the 50 ms RMS level of `first..=last` strays from `target_db`, at worst, in dB.
pub fn level_deviation_db(
    audio: &[Vec<f32>],
    first: usize,
    last: usize,
    target_db: f32,
    rate: u32,
) -> f32 {
    body_levels_db(audio, first, last, rate)
        .iter()
        .map(|d| (d - target_db).abs())
        .fold(0.0, f32::max)
}

/// The spread of the 50 ms RMS level of `first..=last`: loudest frame minus quietest, in dB.
pub fn body_spread_db(audio: &[Vec<f32>], first: usize, last: usize, rate: u32) -> f32 {
    let levels = body_levels_db(audio, first, last, rate);
    let hi = levels.iter().copied().fold(f32::MIN, f32::max);
    let lo = levels.iter().copied().fold(f32::MAX, f32::min);
    if levels.is_empty() { 0.0 } else { hi - lo }
}

/// The mean difference in dB between the 10 ms RMS envelopes of `a` from `a_first` and `b`
/// from `b_first`, over the frames where either is above −60 dBFS. 0 when there are none.
pub fn envelope_error_db(
    a: &[Vec<f32>],
    a_first: usize,
    b: &[Vec<f32>],
    b_first: usize,
    rate: u32,
) -> f32 {
    let hop = frames_in(ENVELOPE_FRAME_S, rate);
    let ea = rms_db_frames(a, a_first, frames(a).saturating_sub(1), hop);
    let eb = rms_db_frames(b, b_first, frames(b).saturating_sub(1), hop);
    let diffs: Vec<f32> = ea
        .iter()
        .zip(&eb)
        .filter(|&(&x, &y)| x > ENVELOPE_FLOOR_DB || y > ENVELOPE_FLOOR_DB)
        .map(|(x, y)| (x - y).abs())
        .collect();
    if diffs.is_empty() {
        0.0
    } else {
        diffs.iter().sum::<f32>() / diffs.len() as f32
    }
}

/// The correlation between the first two channels over `first..=last`:
/// `Σ l·r / √(Σ l² · Σ r²)`. 1 for mono, and 0 when either channel is silent.
pub fn channel_correlation(audio: &[Vec<f32>], first: usize, last: usize) -> f32 {
    let [l, r, ..] = audio else {
        return 1.0;
    };
    let last = last.min(frames(audio).saturating_sub(1));
    if first > last || frames(audio) == 0 {
        return 0.0;
    }
    let (mut lr, mut ll, mut rr) = (0.0f64, 0.0f64, 0.0f64);
    for i in first..=last {
        let (x, y) = (f64::from(l[i]), f64::from(r[i]));
        lr += x * y;
        ll += x * x;
        rr += y * y;
    }
    let den = (ll * rr).sqrt();
    if den > 0.0 { (lr / den) as f32 } else { 0.0 }
}

/// How many extra times the sustain loop plays in a `heard` file.
const HEARD_REPEATS: usize = 3;

/// What the `tail_lab` bench is asked to do (v0.3 §8.3).
#[derive(Clone, Debug)]
pub struct LabArgs {
    /// Where the sample is cut, in seconds. The first new frame is at this time.
    pub cut_s: f32,
    /// Decay or Sustain; it overrides `settings.mode`.
    pub mode: TailMode,
    /// The methods to render, in table order; each overrides `settings.method`.
    pub methods: Vec<TailMethod>,
    /// The other settings: body, decay, release, darken and seed.
    pub settings: TailSettings,
}

/// The lower-case name of a method, as the bench's file names and table use it.
pub fn method_name(m: TailMethod) -> &'static str {
    match m {
        TailMethod::Granular => "granular",
        TailMethod::Smooth => "smooth",
        TailMethod::Harmonic => "harmonic",
    }
}

/// A plain WAV file of `audio` in the document's own format: no `smpl`, no other chunks.
fn plain_wav(doc: &WavDocument, audio: &[Vec<f32>]) -> Result<Vec<u8>, String> {
    let fmt = find(&doc.riff.chunks, b"fmt ")
        .map(|i| doc.riff.chunks[i].data.clone())
        .ok_or("the file has no fmt chunk")?;
    Ok(write_riff(&RiffFile {
        chunks: vec![
            Chunk {
                id: *b"fmt ",
                data: fmt,
            },
            Chunk {
                id: *b"data",
                data: encode_frames(audio, &doc.format),
            },
        ],
        warnings: Vec::new(),
    }))
}

/// What a sampler plays for a loop in `audio`: everything up to the loop end, the loop
/// [`HEARD_REPEATS`] more times, then the rest (the release). `audio` already holds the loop
/// crossfade, as a sampler reads it.
fn unroll_loop(audio: &[Vec<f32>], (start, end): (usize, usize)) -> Vec<Vec<f32>> {
    audio
        .iter()
        .map(|ch| {
            let end = end.min(ch.len().saturating_sub(1));
            let start = start.min(end);
            let mut out = ch[..=end].to_vec();
            for _ in 0..HEARD_REPEATS {
                out.extend_from_slice(&ch[start..=end]);
            }
            out.extend_from_slice(&ch[end + 1..]);
            out
        })
        .collect()
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn dash(v: Option<f32>, prec: usize) -> String {
    v.map_or_else(|| "-".into(), |v| format!("{v:.prec$}"))
}

/// Runs the `tail_lab` bench: cuts the WAV file `input` at `args.cut_s`, renders a tail with
/// each method, writes the files into `out_dir` and returns the table of measurements.
///
/// Files: `original.wav` (a copy of the input), `cut.wav` (the input trimmed to the cut),
/// `tail_<method>.wav` (written through the normal save path, so a sustain loop is in its
/// `smpl` chunk) and, for a Sustain tail with a loop, `tail_<method>_heard.wav`: a plain file
/// with the loop played [`HEARD_REPEATS`] extra times and then the release.
///
/// Any loop in the input is switched off for the cut, so the tail joins at the cut. The
/// numbers are those of v0.3 §8.1, measured against the input. The envelope error compares the
/// tail with the real ending, when the input goes on past the cut. In Sustain that is the body
/// against the real ending, which only agrees for a steady sound. The render time covers
/// `render_tail` alone.
pub fn lab_run(input: &Path, args: &LabArgs, out_dir: &Path) -> Result<String, String> {
    let doc = load_wav(input).map_err(|e| format!("could not load {}: {e}", input.display()))?;
    let rate = doc.format.sample_rate;
    let cut = (f64::from(args.cut_s) * f64::from(rate)).round();
    if !args.cut_s.is_finite() || cut < 1.0 || cut > doc.frames as f64 {
        return Err(format!(
            "the cut ({} s) must be inside the file ({:.3} s)",
            args.cut_s,
            doc.frames as f64 / f64::from(rate.max(1))
        ));
    }
    let cut = cut as usize;
    let mut cut_edit = doc.initial_edit();
    cut_edit.loop_mode = LoopMode::Off;
    cut_edit.start = cut_edit.start.min(cut - 1);
    cut_edit.end = cut - 1;

    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("could not create {}: {e}", out_dir.display()))?;
    std::fs::copy(input, out_dir.join("original.wav"))
        .map_err(|e| format!("could not copy the input: {e}"))?;
    write(&out_dir.join("cut.wav"), &build_wav_bytes(&doc, &cut_edit))?;

    let real_ending = doc.frames > cut;
    let mut table = String::new();
    let mut notes = String::new();
    let mut header = false;
    for &method in &args.methods {
        let name = method_name(method);
        let settings = TailSettings {
            mode: args.mode,
            method,
            ..args.settings
        };
        let started = Instant::now();
        let r = render_tail(&doc.audio, &cut_edit, None, rate, &settings)
            .map_err(|e| format!("{name}: {e}"))?;
        let render_ms = started.elapsed().as_secs_f64() * 1000.0;

        let out = process(&doc.audio, &r.edit.effects);
        let a = &r.analysis;
        if !header {
            header = true;
            let kind = a.character.period.map_or_else(
                || "noisy".to_string(),
                |p| format!("tonal, period {p:.1} frames ({:.1} Hz)", rate as f32 / p),
            );
            let _ = writeln!(
                table,
                "{}: {rate} Hz, {} ch, {:.3} s, cut at {:.3} s (frame {cut}), {:?}",
                input.file_name().map_or_else(
                    || input.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                ),
                doc.format.channels,
                doc.frames as f64 / f64::from(rate),
                cut as f64 / f64::from(rate),
                args.mode,
            );
            let _ = writeln!(
                table,
                "source {:.3}-{:.3} s, {} ({:.1} dB/s), {kind}, level at the cut {:.1} dBFS",
                a.source.0 as f64 / f64::from(rate),
                a.source.1 as f64 / f64::from(rate),
                if a.fit.steady { "steady" } else { "decaying" },
                a.fit.rate_db_s,
                a.level_at_cut_db,
            );
            let _ = writeln!(
                table,
                "{:<9} {:>9} {:>7} {:>6} {:>10} {:>13} {:>6} {:>9} {:>10}",
                "method",
                "render ms",
                "tail s",
                "join",
                "join@start",
                "level step dB",
                "wrap",
                "spread dB",
                "env err dB"
            );
        }

        // The tail's own fall, which the level step allows for.
        let fall = match args.mode {
            TailMode::Decay => {
                let t60 = settings
                    .decay_t60
                    .filter(|t| t.is_finite() && *t > 0.0)
                    .unwrap_or(a.fit.t60.unwrap_or(1.0))
                    .max(0.01);
                60.0 / t60
            }
            TailMode::Sustain => 0.0,
        };
        let join = join_score(&out, cut, &doc.audio, a.source, rate);
        let join_start = join_score(&out, r.step.first, &doc.audio, a.source, rate);
        let step_db = level_step_db(&out, cut, a.level_at_cut_db, rate, fall);
        let looped = (args.mode == TailMode::Sustain && r.edit.loop_mode != LoopMode::Off)
            .then_some((r.edit.loop_start, r.edit.loop_end));
        let wrap = looped.map(|l| wrap_score(&out, l, &doc.audio, a.source, rate));
        let spread = looped.map(|(s, e)| body_spread_db(&out, s, e, rate));
        let env = (real_ending && frames(&out) > cut)
            .then(|| envelope_error_db(&out, cut, &doc.audio, cut, rate));
        let _ = writeln!(
            table,
            "{name:<9} {render_ms:>9.1} {:>7.2} {join:>6.2} {join_start:>10.2} {step_db:>+13.2} {:>6} {:>9} {:>10}",
            (frames(&out) - cut) as f64 / f64::from(rate),
            dash(wrap, 2),
            dash(spread, 2),
            dash(env, 2),
        );
        for w in &r.warnings {
            let _ = writeln!(notes, "{name}: {w:?}");
        }

        write(
            &out_dir.join(format!("tail_{name}.wav")),
            &build_wav_bytes(&doc, &r.edit),
        )?;
        if let Some(l) = looped {
            let heard = unroll_loop(&render(&out, &r.edit), l);
            write(
                &out_dir.join(format!("tail_{name}_heard.wav")),
                &plain_wav(&doc, &heard)?,
            )?;
        }
    }
    table.push_str(&notes);
    let _ = writeln!(table, "files in {}", out_dir.display());
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::load_wav;

    #[test]
    fn measures_known_signals() {
        // A sawtooth: steps of 0.01, and a drop of 0.99 every 100 frames (1 % of the steps).
        let x: Vec<f32> = (0..1000).map(|i| (i % 100) as f32 * 0.01).collect();
        let audio = vec![x];
        let p = step_percentile_99(&audio, 0, 999);
        assert!(p > 0.0 && p < 0.1, "{p}");
        // The first 100 frames hold only small steps, so a drop scores 99 against them.
        let s = join_score(&audio, 300, &audio, (0, 99), 1000);
        assert!(s > 1.5, "a wrap step is a click: {s}");
        let s = join_score(&audio, 350, &audio, (0, 99), 1000);
        assert!(s <= 1.01, "{s}");
        // The reference comes from the input: a louder input halves the score.
        let loud: Vec<Vec<f32>> = vec![audio[0].iter().map(|v| v * 2.0).collect()];
        let s2 = join_score(&audio, 350, &loud, (0, 99), 1000);
        assert!((s2 - s / 2.0).abs() < 1e-4, "{s2} vs {s}");

        // Stereo correlation of a channel with itself, its negative and silence.
        let l: Vec<f32> = (0..500).map(|i| (i as f32 * 0.3).sin()).collect();
        let r: Vec<f32> = l.iter().map(|v| -v).collect();
        assert!((channel_correlation(&[l.clone(), l.clone()], 0, 499) - 1.0).abs() < 1e-6);
        assert!((channel_correlation(&[l.clone(), r], 0, 499) + 1.0).abs() < 1e-6);
        assert_eq!(
            channel_correlation(&[l.clone(), vec![0.0; 500]], 0, 499),
            0.0
        );
        assert_eq!(channel_correlation(std::slice::from_ref(&l), 0, 499), 1.0);

        // A steady sine has no level step and no spread; envelopes agree with themselves.
        let rate = 8000;
        // 16 frames a period, so every window holds whole periods.
        let s: Vec<f32> = (0..8000)
            .map(|i| (i as f32 * std::f32::consts::TAU / 16.0).sin() * 0.5)
            .collect();
        let audio = vec![s];
        let at_cut = level_db(&audio, 4000 - 160, 3999);
        assert!(level_step_db(&audio, 4000, at_cut, rate, 0.0).abs() < 0.1);
        // 6 dB quieter than the level at the cut reads +6.
        assert!((level_step_db(&audio, 4000, at_cut + 6.0, rate, 0.0) - 6.0).abs() < 0.1);
        assert!(body_spread_db(&audio, 0, 7999, rate) < 0.1);
        assert!(level_deviation_db(&audio, 0, 7999, -9.03, rate) < 0.1);
        assert!(envelope_error_db(&audio, 0, &audio, 0, rate) < 1e-6);
        assert!(wrap_score(&audio, (0, 7999), &audio, (0, 7999), rate).is_finite());
    }

    /// A steady 220 Hz tone at 22.05 kHz, written to `dir` as `tone.wav`.
    fn write_tone(dir: &std::path::Path, secs: f32) -> std::path::PathBuf {
        let rate = 22_050;
        let tone = crate::testwav::sine(220.0, rate, (secs * rate as f32) as usize, 0.5);
        let path = dir.join("tone.wav");
        let bytes = crate::testwav::wav_bytes(
            &[tone],
            rate,
            crate::wav::SampleFormat::Int { bits: 16 },
            Vec::new(),
        );
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn lab_args(mode: TailMode) -> LabArgs {
        LabArgs {
            cut_s: 0.6,
            mode,
            methods: vec![
                TailMethod::Granular,
                TailMethod::Smooth,
                TailMethod::Harmonic,
            ],
            settings: TailSettings {
                body_s: 0.5,
                ..TailSettings::default()
            },
        }
    }

    #[test]
    fn tail_lab_writes_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let input = write_tone(dir.path(), 1.0);

        let out = dir.path().join("sustain");
        let table = lab_run(&input, &lab_args(TailMode::Sustain), &out).unwrap();
        for name in ["original", "cut"] {
            load_wav(&out.join(format!("{name}.wav"))).unwrap();
        }
        for m in ["granular", "smooth", "harmonic"] {
            assert!(table.contains(m), "{table}");
            load_wav(&out.join(format!("tail_{m}.wav"))).unwrap();
            let heard = load_wav(&out.join(format!("tail_{m}_heard.wav"))).unwrap();
            assert!(heard.smpl.is_none(), "{m}: heard is a plain file");
        }
        // The cut file stops at the cut; the tail runs on past it with its loop in `smpl`.
        let cut = load_wav(&out.join("cut.wav")).unwrap();
        assert_eq!(cut.frames, (0.6 * 22_050.0f32).round() as usize);
        let tail = load_wav(&out.join("tail_smooth.wav")).unwrap();
        assert!(tail.frames > cut.frames);
        let smpl = tail.smpl.as_ref().expect("smpl chunk");
        assert_eq!(smpl.loops.len(), 1);
        assert_eq!(smpl.loops[0].start as usize, cut.frames);
        let heard = load_wav(&out.join("tail_smooth_heard.wav")).unwrap();
        let body = smpl.loops[0].end as usize + 1 - cut.frames;
        assert!(heard.frames >= tail.frames + 3 * body);

        // Decay writes no loop and no heard file.
        let out = dir.path().join("decay");
        let table = lab_run(&input, &lab_args(TailMode::Decay), &out).unwrap();
        assert!(table.contains("render ms"), "{table}");
        let tail = load_wav(&out.join("tail_smooth.wav")).unwrap();
        assert!(tail.smpl.is_none());
        assert!(!out.join("tail_smooth_heard.wav").exists());
    }

    #[test]
    fn tail_lab_rejects_bad_cuts() {
        let dir = tempfile::tempdir().unwrap();
        let input = write_tone(dir.path(), 1.0);
        let out = dir.path().join("out");
        for cut_s in [0.0, -1.0, f32::NAN, 5.0] {
            let args = LabArgs {
                cut_s,
                ..lab_args(TailMode::Decay)
            };
            assert!(lab_run(&input, &args, &out).is_err(), "cut {cut_s}");
        }
        let missing = dir.path().join("none.wav");
        assert!(lab_run(&missing, &lab_args(TailMode::Decay), &out).is_err());
    }

    #[test]
    fn measures_handle_degenerate_input() {
        let empty: Vec<Vec<f32>> = vec![];
        assert_eq!(step_percentile_99(&empty, 0, 10), 0.0);
        assert_eq!(join_score(&empty, 0, &empty, (0, 10), 8000), 0.0);
        assert_eq!(wrap_score(&empty, (0, 10), &empty, (0, 10), 8000), 0.0);
        assert!(level_step_db(&empty, 0, -20.0, 8000, 10.0).is_finite());
        assert_eq!(body_spread_db(&empty, 0, 10, 8000), 0.0);
        assert_eq!(envelope_error_db(&empty, 0, &empty, 0, 8000), 0.0);
        let one = vec![vec![0.5f32]];
        assert_eq!(join_score(&one, 0, &one, (0, 0), 8000), 0.0);
        assert_eq!(channel_correlation(&[vec![], vec![]], 0, 10), 0.0);
    }
}
