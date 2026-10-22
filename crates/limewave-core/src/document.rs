//! A loaded WAV file: decoded audio, parsed metadata and the initial edit state.

use std::path::{Path, PathBuf};

use crate::cue::check_markers;
use crate::edit::{EditState, LoopMode};
use crate::error::{LoadError, WavError};
use crate::meta::{info_get, info_warnings, read_acid, read_inst};
use crate::riff::{Chunk, RiffFile, find, parse_riff, write_riff};
use crate::smpl::{Smpl, SmplLoop, sample_period, smpl_to_tuning};
use crate::wav::{SampleFormat, WavFormat, decode, encode_frames, fmt_bytes, parse_fmt};

/// A WAV file as read from disk. The original chunks are kept in `riff` so that unknown
/// chunks survive a save.
#[derive(Debug, Clone)]
pub struct WavDocument {
    pub path: PathBuf,
    pub riff: RiffFile,
    pub format: WavFormat,
    /// Planar audio, `[channel][frame]`.
    pub audio: Vec<Vec<f32>>,
    pub frames: usize,
    pub smpl: Option<Smpl>,
    pub name: Option<String>,
    pub software: Option<String>,
    pub warnings: Vec<String>,
}

/// The largest file a RIFF header can describe: a 32-bit size plus the 8-byte header.
const MAX_FILE_LEN: u64 = u32::MAX as u64 + 8;

/// Refuses a file longer than a RIFF size can describe.
fn check_file_len(len: u64) -> Result<(), LoadError> {
    if len > MAX_FILE_LEN {
        return Err(LoadError::TooLarge(len));
    }
    Ok(())
}

/// Reads and decodes the WAV file at `path`. Files over 4 GiB are refused before reading.
pub fn load_wav(path: &Path) -> Result<WavDocument, LoadError> {
    check_file_len(std::fs::metadata(path)?.len())?;
    let bytes = std::fs::read(path)?;
    load_wav_bytes(path.to_path_buf(), &bytes)
}

/// Decodes WAV file contents; `path` is only recorded in the result.
pub fn load_wav_bytes(path: PathBuf, bytes: &[u8]) -> Result<WavDocument, LoadError> {
    let riff = parse_riff(bytes)?;
    let mut warnings = riff.warnings.clone();

    let fmt_idx = find(&riff.chunks, b"fmt ")
        .ok_or_else(|| WavError::Malformed("missing fmt chunk".into()))?;
    let data_idx = find(&riff.chunks, b"data")
        .ok_or_else(|| WavError::Malformed("missing data chunk".into()))?;

    let format = parse_fmt(&riff.chunks[fmt_idx].data)?;
    if format.channels > 2 {
        return Err(WavError::TooManyChannels(format.channels).into());
    }
    let audio = decode(&riff.chunks[data_idx].data, &format);
    let frames = audio.first().map_or(0, Vec::len);

    let smpl = find(&riff.chunks, b"smpl").and_then(|i| match Smpl::parse(&riff.chunks[i].data) {
        Ok(s) => Some(s),
        Err(e) => {
            warnings.push(format!("ignoring unreadable smpl chunk: {e}"));
            None
        }
    });
    warnings.extend(derive_edit(frames, smpl.as_ref(), &riff.chunks).1);

    warnings.extend(info_warnings(&riff.chunks));
    let name = info_get(&riff.chunks, b"INAM");
    let software = info_get(&riff.chunks, b"ISFT");
    if check_markers(&riff.chunks).is_err() {
        warnings.push("cue markers could not be read; they will be saved unchanged".into());
    }

    Ok(WavDocument {
        path,
        riff,
        format,
        audio,
        frames,
        smpl,
        name,
        software,
        warnings,
    })
}

/// The root note and tuning of a file without a `smpl` chunk: from the first `inst` chunk
/// (root and fine tune), else from the first `acid` chunk (root only), read the way
/// [`crate::meta::sync_inst`] and [`crate::meta::sync_acid`] write them.
fn fallback_tuning(chunks: &[Chunk]) -> Option<(u8, f32)> {
    let body = |id: &[u8; 4]| find(chunks, id).map(|i| chunks[i].data.as_slice());
    body(b"inst")
        .and_then(read_inst)
        .or_else(|| body(b"acid").and_then(read_acid).map(|root| (root, 0.0)))
}

/// A document that exists only in memory (a blank sample or a recording), named `path` but not
/// yet on disk. `audio` is 1 or 2 channels of equal, non-zero length; callers check that first.
/// The `smpl` chunk carries `root` and, with `whole_loop`, a forward loop over every frame, so
/// the document's initial edit has them. Float files also get a `fact` chunk.
pub fn new_document(
    path: PathBuf,
    audio: Vec<Vec<f32>>,
    rate: u32,
    format: SampleFormat,
    root: u8,
    whole_loop: bool,
) -> WavDocument {
    let frames = audio.first().map_or(0, Vec::len);
    assert!(
        (1..=2).contains(&audio.len()) && frames > 0 && audio.iter().all(|c| c.len() == frames),
        "a new document needs 1 or 2 equal, non-empty channels"
    );
    let (SampleFormat::Int { bits } | SampleFormat::Float { bits }) = format;
    let channels = audio.len() as u16;
    let fmt = WavFormat {
        channels,
        sample_rate: rate,
        format,
        block_align: channels * (bits / 8),
    };
    let frame_count = u32::try_from(frames).unwrap_or(u32::MAX);
    let smpl = Smpl {
        manufacturer: 0,
        product: 0,
        sample_period: sample_period(rate),
        unity_note: u32::from(root),
        pitch_fraction: 0,
        smpte_format: 0,
        smpte_offset: 0,
        loops: if whole_loop {
            vec![SmplLoop {
                cue_id: 0,
                loop_type: 0,
                start: 0,
                end: frame_count - 1,
                fraction: 0,
                play_count: 0,
            }]
        } else {
            Vec::new()
        },
        sampler_data: Vec::new(),
    };
    let mut chunks = vec![Chunk {
        id: *b"fmt ",
        data: fmt_bytes(&fmt),
    }];
    if matches!(format, SampleFormat::Float { .. }) {
        chunks.push(Chunk {
            id: *b"fact",
            data: frame_count.to_le_bytes().to_vec(),
        });
    }
    chunks.push(Chunk {
        id: *b"smpl",
        data: smpl.to_bytes(),
    });
    chunks.push(Chunk {
        id: *b"data",
        data: encode_frames(&audio, &fmt),
    });
    let bytes = write_riff(&RiffFile {
        chunks,
        warnings: Vec::new(),
    });
    load_wav_bytes(path, &bytes).expect("a document built from its own bytes loads")
}

/// The edit state implied by a file's `smpl` chunk (root note, tuning and the first loop),
/// plus the warnings raised while converting it. Without a `smpl` chunk, the root and tuning
/// come from `inst` or `acid` in `chunks`, when present.
fn derive_edit(frames: usize, smpl: Option<&Smpl>, chunks: &[Chunk]) -> (EditState, Vec<String>) {
    let mut warnings = Vec::new();
    let mut e = EditState::default_for(frames);
    let Some(smpl) = smpl else {
        if let Some((root, cents)) = fallback_tuning(chunks) {
            (e.root, e.cents) = (root, cents);
        }
        return (e, warnings);
    };

    let (root, cents) = smpl_to_tuning(smpl.unity_note, smpl.pitch_fraction);
    e.root = root;
    e.cents = cents;

    let Some(l) = smpl.loops.first() else {
        return (e.clamped(frames), warnings);
    };
    e.loop_mode = match l.loop_type {
        0 => LoopMode::Forward,
        1 => LoopMode::PingPong,
        2 => LoopMode::Backward,
        other => {
            warnings.push(format!("unknown loop type {other} shown as forward"));
            LoopMode::Forward
        }
    };
    let (start, mut end) = (l.start as usize, l.end as usize);
    // Writers that store an exclusive end give one past the last frame: pull it back.
    if frames > 0 && end == frames {
        end = frames - 1;
        warnings.push("loop end was one past the last sample; clamped".into());
    }
    e.loop_start = start;
    e.loop_end = end;

    let mut e = e.clamped(frames);
    if start > end || e.loop_mode == LoopMode::Off || e.loop_start != start || e.loop_end != end {
        e.loop_mode = LoopMode::Off;
        warnings.push("loop in file was out of range and has been disabled".into());
    }
    (e, warnings)
}

impl WavDocument {
    /// The edit state implied by the file: root note, tuning and the first `smpl` loop (the
    /// root and tuning from `inst` or `acid` when there is no `smpl`). The warnings this
    /// raises are already in `self.warnings`.
    pub fn initial_edit(&self) -> EditState {
        derive_edit(self.frames, self.smpl.as_ref(), &self.riff.chunks).0
    }

    /// The raw bytes of the `data` chunk.
    pub fn data_chunk(&self) -> &[u8] {
        find(&self.riff.chunks, b"data").map_or(&[], |i| &self.riff.chunks[i].data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::riff::Chunk;
    use crate::save::{Destination, save_wav};
    use crate::testwav::{sine, wav_bytes};
    use crate::wav::SampleFormat;

    const PCM16: SampleFormat = SampleFormat::Int { bits: 16 };

    fn smpl_chunk(unity: u32, loops: &[(u32, u32, u32)]) -> Chunk {
        let mut d = Vec::new();
        for v in [0, 0, 45351, unity, 0, 0, 0, loops.len() as u32, 0] {
            d.extend_from_slice(&u32::to_le_bytes(v));
        }
        for &(ty, s, e) in loops {
            for v in [0, ty, s, e, 0, 0] {
                d.extend_from_slice(&u32::to_le_bytes(v));
            }
        }
        Chunk {
            id: *b"smpl",
            data: d,
        }
    }

    fn sub(id: &[u8; 4], text: &str) -> Vec<u8> {
        let mut v = id.to_vec();
        let mut body = text.as_bytes().to_vec();
        body.push(0);
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(&body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn load(frames: usize, extra: Vec<Chunk>) -> Result<WavDocument, LoadError> {
        let bytes = wav_bytes(&[sine(440.0, 22050, frames, 0.5)], 22050, PCM16, extra);
        load_wav_bytes(PathBuf::from("t.wav"), &bytes)
    }

    #[test]
    fn loads_mono_16bit_with_smpl_loop() {
        let doc = load(2927, vec![smpl_chunk(60, &[(0, 2532, 2915)])]).unwrap();
        assert_eq!(doc.frames, 2927);
        assert_eq!(doc.format.sample_rate, 22050);
        assert_eq!(doc.audio.len(), 1);
        assert!(doc.warnings.is_empty());
        let e = doc.initial_edit();
        assert_eq!(e.loop_mode, LoopMode::Forward);
        assert_eq!((e.loop_start, e.loop_end), (2532, 2915));
        assert_eq!((e.root, e.cents), (60, 0.0));
    }

    #[test]
    fn loads_one_shot_smpl_zero_loops() {
        let doc = load(1000, vec![smpl_chunk(62, &[])]).unwrap();
        let e = doc.initial_edit();
        assert_eq!(e.loop_mode, LoopMode::Off);
        assert_eq!(e.root, 62);
    }

    #[test]
    fn no_smpl_gives_defaults() {
        let doc = load(1000, vec![]).unwrap();
        assert!(doc.smpl.is_none());
        let e = doc.initial_edit();
        assert_eq!(e.root, 60);
        assert_eq!(e.loop_mode, LoopMode::Off);
        assert_eq!(e.end, 999);
    }

    #[test]
    fn ping_pong_loop_type() {
        let doc = load(1000, vec![smpl_chunk(60, &[(1, 100, 500)])]).unwrap();
        assert_eq!(doc.initial_edit().loop_mode, LoopMode::PingPong);
        assert!(doc.warnings.is_empty());
    }

    #[test]
    fn backward_loop_type_reads_as_backward() {
        let doc = load(1000, vec![smpl_chunk(60, &[(2, 100, 500)])]).unwrap();
        let e = doc.initial_edit();
        assert_eq!(e.loop_mode, LoopMode::Backward);
        assert_eq!((e.loop_start, e.loop_end), (100, 500));
        assert!(doc.warnings.is_empty());
    }

    #[test]
    fn reads_info_and_markers() {
        let mut list = b"INFO".to_vec();
        list.extend(sub(b"INAM", "Pad"));
        list.extend(sub(b"ISFT", "Tool 1.0"));
        let extra = vec![
            Chunk {
                id: *b"LIST",
                data: list,
            },
            Chunk {
                id: *b"cue ",
                data: vec![0; 4],
            },
        ];
        let doc = load(100, extra).unwrap();
        assert_eq!(doc.name.as_deref(), Some("Pad"));
        assert_eq!(doc.software.as_deref(), Some("Tool 1.0"));
        assert!(doc.warnings.is_empty(), "{:?}", doc.warnings);
    }

    #[test]
    fn unreadable_info_list_is_reported() {
        let mut list = b"INFO".to_vec();
        list.extend(sub(b"INAM", "Pad"));
        list.extend_from_slice(&[1, 2, 3, 4, 0xff, 0xff, 0, 0, 9, 9]);
        let extra = vec![Chunk {
            id: *b"LIST",
            data: list,
        }];
        let doc = load(100, extra).unwrap();
        assert_eq!(doc.name.as_deref(), Some("Pad"));
        assert_eq!(doc.warnings.len(), 1);
        assert!(doc.warnings[0].contains("LIST/INFO"), "{:?}", doc.warnings);
    }

    #[test]
    fn unreadable_cue_warns_and_loads() {
        const WARNING: &str = "cue markers could not be read; they will be saved unchanged";
        // A count of 5 with no records behind it.
        let bad = Chunk {
            id: *b"cue ",
            data: vec![5, 0, 0, 0],
        };
        let doc = load(100, vec![bad]).unwrap();
        assert_eq!(doc.frames, 100);
        assert_eq!(doc.warnings, vec![WARNING]);

        // An `adtl` entry that runs past its list.
        let broken = Chunk {
            id: *b"LIST",
            data: b"adtllabl\xff\0\0\0\x01\0\0\0".to_vec(),
        };
        assert_eq!(load(100, vec![broken]).unwrap().warnings, vec![WARNING]);

        // Readable markers raise nothing.
        let good = vec![
            Chunk {
                id: *b"cue ",
                data: vec![0; 4],
            },
            Chunk {
                id: *b"LIST",
                data: b"adtl".to_vec(),
            },
        ];
        assert!(load(100, good).unwrap().warnings.is_empty());
    }

    #[test]
    fn rejects_three_channels() {
        let ch = vec![sine(440.0, 22050, 10, 0.5); 3];
        let bytes = wav_bytes(&ch, 22050, PCM16, vec![]);
        let err = load_wav_bytes(PathBuf::from("t.wav"), &bytes).unwrap_err();
        assert!(matches!(err, LoadError::Wav(WavError::TooManyChannels(3))));
    }

    #[test]
    fn initial_edit_clamps_out_of_range_loop() {
        let doc = load(1000, vec![smpl_chunk(60, &[(0, 100, 5000)])]).unwrap();
        let e = doc.initial_edit();
        assert_eq!(e.loop_mode, LoopMode::Off);
        assert!(e.loop_start <= e.loop_end && e.loop_end <= e.end);
        assert!(
            doc.warnings
                .contains(&"loop in file was out of range and has been disabled".to_string())
        );
    }

    #[test]
    fn loop_end_one_past_the_last_frame_is_clamped() {
        let doc = load(1000, vec![smpl_chunk(60, &[(0, 100, 1000)])]).unwrap();
        let e = doc.initial_edit();
        assert_eq!(e.loop_mode, LoopMode::Forward);
        assert_eq!((e.loop_start, e.loop_end), (100, 999));
        assert_eq!(
            doc.warnings,
            vec!["loop end was one past the last sample; clamped"]
        );
    }

    #[test]
    fn inverted_loop_is_disabled() {
        let doc = load(1000, vec![smpl_chunk(60, &[(0, 500, 100)])]).unwrap();
        assert_eq!(doc.initial_edit().loop_mode, LoopMode::Off);
        assert_eq!(doc.warnings.len(), 1);
    }

    #[test]
    fn malformed_smpl_warns_and_loads() {
        let bad = Chunk {
            id: *b"smpl",
            data: vec![0; 10],
        };
        let doc = load(100, vec![bad]).unwrap();
        assert!(doc.smpl.is_none());
        assert_eq!(doc.warnings.len(), 1);
        assert_eq!(doc.initial_edit().loop_mode, LoopMode::Off);
    }

    #[test]
    fn zero_frame_file_loads() {
        let doc = load(0, vec![]).unwrap();
        assert_eq!(doc.frames, 0);
        let e = doc.initial_edit();
        assert_eq!((e.start, e.end), (0, 0));
        let doc = load(0, vec![smpl_chunk(60, &[(0, 0, 10)])]).unwrap();
        assert_eq!(doc.initial_edit().loop_mode, LoopMode::Off);
    }

    #[test]
    fn missing_data_chunk_is_error() {
        let bytes = wav_bytes(&[sine(440.0, 22050, 10, 0.5)], 22050, PCM16, vec![]);
        let mut riff = parse_riff(&bytes).unwrap();
        riff.chunks.retain(|c| &c.id != b"data");
        let bytes = crate::riff::write_riff(&riff);
        let err = load_wav_bytes(PathBuf::from("t.wav"), &bytes).unwrap_err();
        assert!(matches!(err, LoadError::Wav(WavError::Malformed(_))));
    }

    #[test]
    fn files_over_4_gib_are_refused_before_reading() {
        let limit = u64::from(u32::MAX) + 8;
        assert!(check_file_len(limit).is_ok());
        let err = check_file_len(limit + 1).unwrap_err();
        assert!(
            matches!(err, LoadError::TooLarge(n) if n == limit + 1),
            "{err}"
        );
        assert!(err.to_string().contains("4 GiB"), "{err}");
    }

    #[test]
    fn data_chunk_returns_raw_bytes() {
        let doc = load(50, vec![]).unwrap();
        assert_eq!(doc.data_chunk().len(), 100);
    }

    #[test]
    fn load_wav_reads_a_file_and_reports_io_errors() {
        let bytes = wav_bytes(&[sine(440.0, 22050, 10, 0.5)], 22050, PCM16, vec![]);
        let path = std::env::temp_dir().join("limewave_document_test.wav");
        std::fs::write(&path, &bytes).unwrap();
        let doc = load_wav(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(doc.frames, 10);
        assert_eq!(doc.path, path);
        let missing = std::env::temp_dir().join("limewave_no_such_file.wav");
        assert!(matches!(load_wav(&missing), Err(LoadError::Io(_))));
    }

    fn float32() -> SampleFormat {
        SampleFormat::Float { bits: 32 }
    }

    fn save_to(doc: &WavDocument, e: &EditState, path: &Path) -> WavDocument {
        save_wav(doc, e, &Destination::File(path.to_path_buf())).unwrap();
        load_wav(path).unwrap()
    }

    #[test]
    fn new_document_saves_and_reloads_silence() {
        let dir = tempfile::tempdir().unwrap();
        for ch in 1..=2usize {
            for fmt in [
                SampleFormat::Int { bits: 16 },
                SampleFormat::Int { bits: 24 },
                float32(),
            ] {
                for rate in [44_100, 48_000] {
                    let doc = new_document(
                        "Untitled 1.wav".into(),
                        vec![vec![0.0; 441]; ch],
                        rate,
                        fmt,
                        60,
                        false,
                    );
                    let path = dir.path().join("a.wav");
                    let back = save_to(&doc, &doc.initial_edit(), &path);
                    assert_eq!(back.frames, 441);
                    assert!(back.audio.iter().flatten().all(|&v| v == 0.0));
                    assert_eq!(usize::from(back.format.channels), ch);
                    assert_eq!(back.format.sample_rate, rate);
                    assert_eq!(back.format.format, fmt);
                    assert_eq!(back.smpl.as_ref().map(|s| s.unity_note), Some(60));
                    assert_eq!(back.initial_edit().loop_mode, LoopMode::Off);
                    let bytes = std::fs::read(&path).unwrap();
                    assert!(bytes.windows(4).any(|w| w == b"ISFT"));
                    let fact = find(&back.riff.chunks, b"fact");
                    if fmt == float32() {
                        let i = fact.expect("float files carry a fact chunk");
                        assert_eq!(back.riff.chunks[i].data[..4], 441u32.to_le_bytes());
                    }
                }
            }
        }
    }

    #[test]
    fn new_document_whole_loop_and_root() {
        let doc = new_document(
            "Untitled 1.wav".into(),
            vec![vec![0.0; 100]],
            44_100,
            PCM16,
            69,
            true,
        );
        let check = |e: EditState| {
            assert_eq!(e.root, 69);
            assert_eq!(e.loop_mode, LoopMode::Forward);
            assert_eq!((e.loop_start, e.loop_end), (0, 99));
        };
        check(doc.initial_edit());
        let dir = tempfile::tempdir().unwrap();
        let back = save_to(&doc, &doc.initial_edit(), &dir.path().join("a.wav"));
        check(back.initial_edit());
    }

    #[test]
    fn a_recorded_take_round_trips_within_precision() {
        let take = sine(440.0, 48_000, 4800, 0.5);
        let dir = tempfile::tempdir().unwrap();
        for (fmt, tol) in [
            (PCM16, 2.0 / 32768.0),
            (SampleFormat::Int { bits: 24 }, 2.0 / 8_388_608.0),
            (float32(), 0.0),
        ] {
            let doc = new_document(
                "Recording 1.wav".into(),
                vec![take.clone()],
                48_000,
                fmt,
                60,
                false,
            );
            let back = save_to(&doc, &doc.initial_edit(), &dir.path().join("a.wav"));
            assert_eq!(back.frames, take.len());
            for (a, b) in back.audio[0].iter().zip(&take) {
                assert!((a - b).abs() <= tol, "{fmt:?}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn redrawn_samples_on_a_new_document_are_saved() {
        let doc = new_document(
            "Untitled 1.wav".into(),
            vec![vec![0.0; 100]],
            44_100,
            PCM16,
            60,
            false,
        );
        let e = crate::edit::redraw_samples(&doc.initial_edit(), &doc.audio, 0, 10, &[0.5, -0.5])
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let back = save_to(&doc, &e, &dir.path().join("a.wav"));
        let tol = 2.0 / 32768.0;
        for (n, &v) in back.audio[0].iter().enumerate() {
            let want = match n {
                10 => 0.5,
                11 => -0.5,
                _ => 0.0,
            };
            assert!((v - want).abs() <= tol, "sample {n}: {v}");
        }
    }

    #[test]
    fn file_destination_backs_up_a_different_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        std::fs::write(&path, b"junk").unwrap();
        let doc = new_document(
            "Untitled 1.wav".into(),
            vec![vec![0.0; 10]],
            44_100,
            PCM16,
            60,
            false,
        );
        let out = save_wav(&doc, &doc.initial_edit(), &Destination::File(path.clone())).unwrap();
        assert_eq!(out.written, path);
        let backup = out.backup.expect("the existing file is backed up");
        assert_eq!(std::fs::read(backup).unwrap(), b"junk");
        assert_eq!(load_wav(&path).unwrap().frames, 10);
    }
}
