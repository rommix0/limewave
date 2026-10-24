//! The `loop_lab` bench: runs the loop finder on a WAV file, prints the candidates beside the
//! file's stored loop and writes each loop out to listen to.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use super::steady::search_range;
use super::{
    Direction, FinderSettings, MAX_CANDIDATES, SeamMeasures, find_loops, natural_change, quality,
    score, seam_measures,
};
use crate::document::{WavDocument, load_wav};
use crate::edit::LoopMode;
use crate::riff::{Chunk, RiffFile, find, write_riff};
use crate::wav::encode_frames;

/// How many times each listening file plays its loop.
const PASSES: usize = 3;

/// The lead-in before the first pass of a listening file, in seconds.
const LEAD_IN_S: f64 = 1.0;

/// What the `loop_lab` bench is asked to do.
#[derive(Clone, Debug)]
pub struct LabArgs {
    /// A loop end to keep fixed, in frames, or `None` to search both points.
    pub keep_end: Option<usize>,
    /// The first and last frame to search (inclusive), or `None` for the steady part.
    pub range: Option<(usize, usize)>,
    /// The shortest loop, in seconds.
    pub min_s: f64,
    /// The longest loop, in seconds, or `None` for no limit.
    pub max_s: Option<f64>,
    /// How much to favour long loops: 0 favours short, 1 favours long, 0.5 is neutral.
    pub preference: f32,
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

/// What a listener should hear for the loop `start..=end`: up to [`LEAD_IN_S`] of the audio
/// before the start, then the loop [`PASSES`] times.
fn passes(audio: &[Vec<f32>], start: usize, end: usize, rate: u32) -> Vec<Vec<f32>> {
    let lead = (LEAD_IN_S * f64::from(rate)) as usize;
    audio
        .iter()
        .map(|ch| {
            let mut out = ch[start.saturating_sub(lead)..start].to_vec();
            for _ in 0..PASSES {
                out.extend_from_slice(&ch[start..=end]);
            }
            out
        })
        .collect()
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Appends one table row: the label, the loop points, the length, the cycles at `period`, the
/// quality, the measures and the seam score.
fn row(
    table: &mut String,
    label: &str,
    (start, end): (usize, usize),
    period: Option<f32>,
    m: &SeamMeasures,
    rate: u32,
) {
    let len = end + 1 - start;
    let cycles = period.map_or_else(|| "-".into(), |p| format!("{:.2}", len as f32 / p));
    let _ = writeln!(
        table,
        "{label:>6} {start:>9} {end:>9} {len:>9} {:>8.3} {cycles:>7} {:<9} {:>8.4} {:>7.4} {:>8.5} {:>7.4}",
        len as f64 / f64::from(rate),
        format!("{:?}", quality(m)),
        m.click,
        m.tone,
        m.matching,
        score(m),
    );
}

/// Runs the `loop_lab` bench: searches the WAV file `input` for forward loops, writes a
/// listening file for each into `out_dir` and returns the table of candidates.
///
/// - The range is `args.range`, or else the steady part from [`search_range`].
/// - The header gives the range, the period and the sound's own change ([`natural_change`]).
/// - The table has one row per candidate, best first, then a `stored` row with the measures of
///   the file's own loop, if it has one, measured as a forward loop against the same range. It
///   ends with the search time, which covers [`find_loops`] alone.
/// - The files are `candidate_<n>.wav` and `stored.wav`: plain files in the input's format,
///   with up to a second of lead-in and then the loop three times. Older files of those names
///   are removed first, so the folder holds only this run's loops.
///
/// # Errors
///
/// A message when the file cannot be read, the finder refuses the settings, or a file cannot be
/// written.
pub fn lab_run(input: &Path, args: &LabArgs, out_dir: &Path) -> Result<String, String> {
    let doc = load_wav(input).map_err(|e| format!("could not load {}: {e}", input.display()))?;
    let rate = doc.format.sample_rate;
    if doc.frames == 0 {
        return Err(format!("{} has no audio", input.display()));
    }
    let seconds = |frames: usize| frames as f64 / f64::from(rate);
    let to_frames = |s: f64| (s * f64::from(rate)).round().max(0.0) as usize;
    let (range, kind) = match args.range {
        Some(r) => (r, "given".to_string()),
        None => {
            let r = search_range(&doc.audio, rate, None, (0, doc.frames - 1))
                .map_err(|e| format!("no range to search: {e:?}"))?;
            ((r.first, r.last), format!("{:?}", r.kind))
        }
    };
    let settings = FinderSettings {
        range,
        keep_end: args.keep_end,
        min_len: to_frames(args.min_s),
        max_len: args.max_s.map(to_frames),
        preference: args.preference,
        direction: Direction::Forward,
        max_candidates: MAX_CANDIDATES,
    };
    let started = Instant::now();
    let found = find_loops(&doc.audio, rate, &settings, &AtomicBool::new(false))
        .map_err(|e| format!("the finder failed: {e:?}"))?;
    let search_ms = started.elapsed().as_secs_f64() * 1000.0;
    let period = found.first().and_then(|c| c.period);

    let mut table = String::new();
    let _ = writeln!(
        table,
        "{}: {rate} Hz, {} ch, {:.3} s; range {}-{} ({:.3}-{:.3} s, {kind}){}",
        input.file_name().map_or_else(
            || input.display().to_string(),
            |n| n.to_string_lossy().into_owned()
        ),
        doc.format.channels,
        seconds(doc.frames),
        range.0,
        range.1,
        seconds(range.0),
        seconds(range.1),
        args.keep_end
            .map_or_else(String::new, |e| format!("; loop end kept at {e}")),
    );
    let natural = natural_change(&doc.audio, range.0, range.1, rate);
    let _ = writeln!(
        table,
        "period {}; natural change: second difference {:.3e}, band levels {:.3} dB",
        period.map_or_else(|| "-".into(), |p| format!("{p:.1} frames")),
        natural.d2,
        natural.flux,
    );
    let _ = writeln!(
        table,
        "{:>6} {:>9} {:>9} {:>9} {:>8} {:>7} {:<9} {:>8} {:>7} {:>8} {:>7}",
        "rank",
        "start",
        "end",
        "frames",
        "seconds",
        "cycles",
        "quality",
        "click",
        "tone",
        "matching",
        "score"
    );

    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("could not create {}: {e}", out_dir.display()))?;
    let old = (1..=MAX_CANDIDATES)
        .map(|n| format!("candidate_{n}.wav"))
        .chain(std::iter::once("stored.wav".to_string()));
    for name in old {
        let path = out_dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| format!("could not remove {}: {e}", path.display()))?;
        }
    }

    for (n, c) in found.iter().enumerate() {
        let (s, e) = (c.loop_start, c.loop_end);
        row(
            &mut table,
            &(n + 1).to_string(),
            (s, e),
            period,
            &c.measures,
            rate,
        );
        write(
            &out_dir.join(format!("candidate_{}.wav", n + 1)),
            &plain_wav(&doc, &passes(&doc.audio, s, e, rate))?,
        )?;
    }
    let stored = doc.initial_edit();
    if stored.loop_mode != LoopMode::Off
        && stored.loop_start < stored.loop_end
        && stored.loop_end < doc.frames
    {
        let (s, e) = (stored.loop_start, stored.loop_end);
        let m = seam_measures(&doc.audio, s, e, rate, &natural);
        row(&mut table, "stored", (s, e), period, &m, rate);
        write(
            &out_dir.join("stored.wav"),
            &plain_wav(&doc, &passes(&doc.audio, s, e, rate))?,
        )?;
    }
    let _ = writeln!(table, "search {search_ms:.0} ms");
    let _ = writeln!(table, "files in {}", out_dir.display());
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{RATE, TILE, stereo_repeat};
    use super::*;
    use crate::riff::Chunk;
    use crate::smpl::{Smpl, SmplLoop};
    use crate::testwav::wav_bytes;
    use crate::wav::SampleFormat;

    #[test]
    fn lab_run_writes_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let smpl = Smpl {
            manufacturer: 0,
            product: 0,
            sample_period: 22_676,
            unity_note: 60,
            pitch_fraction: 0,
            smpte_format: 0,
            smpte_offset: 0,
            loops: vec![SmplLoop {
                cue_id: 0,
                loop_type: 0,
                start: TILE as u32,
                end: 2 * TILE as u32 - 1,
                fraction: 0,
                play_count: 0,
            }],
            sampler_data: Vec::new(),
        };
        let bytes = wav_bytes(
            &stereo_repeat(),
            RATE,
            SampleFormat::Int { bits: 16 },
            vec![Chunk {
                id: *b"smpl",
                data: smpl.to_bytes(),
            }],
        );
        let input = dir.path().join("repeat.wav");
        std::fs::write(&input, bytes).unwrap();
        let out = dir.path().join("out");
        let args = LabArgs {
            keep_end: None,
            range: None,
            min_s: 0.1,
            max_s: None,
            preference: 0.5,
        };
        let table = lab_run(&input, &args, &out).unwrap();
        assert!(table.contains("stored"), "{table}");
        assert!(table.contains("Seamless"), "{table}");
        assert!(out.join("candidate_1.wav").exists());
        assert!(out.join("stored.wav").exists());
    }
}
