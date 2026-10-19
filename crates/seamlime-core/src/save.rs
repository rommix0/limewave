//! Saving: builds the edited WAV bytes (crossfade, trim, `smpl`, `ISFT`) and writes them
//! atomically, with numbered backups when overwriting.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::document::WavDocument;
use crate::edit::{EditState, LoopMode};
use crate::error::SaveError;
use crate::meta::{info_set, sync_acid, sync_inst};
use crate::render::crossfade_region;
use crate::riff::{Chunk, RiffFile, list_type, write_riff};
use crate::smpl::{Smpl, SmplLoop, sample_period, tuning_to_smpl};
use crate::software_tag;
use crate::wav::{encode_frames, frame_stride};

/// Where a save goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Into this folder (created if missing), keeping the file name. A different file of
    /// that name already there is first copied to a numbered `.bak` next to it.
    Folder(PathBuf),
    /// Over the original file, after copying it to a numbered `.bak` (see [`backup_path`]).
    OverwriteWithBackup,
    /// Over `doc.path` itself, without a backup. Only for a document that was reopened
    /// from a file this program saved, when it is saved again to the same place.
    ReplaceSaved,
}

/// What a successful save wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveOutcome {
    pub written: PathBuf,
    pub backup: Option<PathBuf>,
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The `data` bytes: the original bytes with only the crossfade region re-encoded, then
/// trimmed to frames `e.start..=e.end`. `e` must be clamped.
fn edited_data(doc: &WavDocument, e: &EditState) -> Vec<u8> {
    if doc.frames == 0 {
        return Vec::new();
    }
    let stride = frame_stride(&doc.format);
    let mut data = doc.data_chunk().to_vec();
    if let Some((first, frames)) = crossfade_region(&doc.audio, e) {
        let encoded = encode_frames(&frames, &doc.format);
        let at = (first * stride).min(data.len());
        let to = (at + encoded.len()).min(data.len());
        data[at..to].copy_from_slice(&encoded[..to - at]);
    }
    let from = (e.start * stride).min(data.len());
    let to = ((e.end + 1) * stride).min(data.len());
    data.truncate(to);
    data.drain(..from);
    data
}

/// Whether a file without a `smpl` chunk needs one: the tuning or the loop changed.
/// Loop points only count while the loop is on.
fn tuning_or_loop_changed(init: &EditState, e: &EditState) -> bool {
    let loop_changed = e.loop_mode != init.loop_mode
        || (e.loop_mode != LoopMode::Off
            && (e.loop_start, e.loop_end) != (init.loop_start, init.loop_end));
    e.root != init.root || e.cents != init.cents || loop_changed
}

/// The `smpl` chunk for `e` (clamped), keeping the original's other fields and those of
/// its extra loops that lie inside the trim range (shifted by `-e.start`).
///
/// Loop 0 is type 0 (forward) or 1 (ping-pong), except that an original backward or
/// unknown type (2 and up, shown as forward) is kept while the loop mode and points
/// are unchanged from `init`.
fn edited_smpl(doc: &WavDocument, init: &EditState, e: &EditState) -> Smpl {
    let mut s = doc.smpl.clone().unwrap_or(Smpl {
        manufacturer: 0,
        product: 0,
        sample_period: 0,
        unity_note: 0,
        pitch_fraction: 0,
        smpte_format: 0,
        smpte_offset: 0,
        loops: Vec::new(),
        sampler_data: Vec::new(),
    });
    s.sample_period = sample_period(doc.format.sample_rate);
    (s.unity_note, s.pitch_fraction) = tuning_to_smpl(e.root, e.cents);

    let loop_unchanged =
        (e.loop_mode, e.loop_start, e.loop_end) == (init.loop_mode, init.loop_start, init.loop_end);
    let kept_type = s
        .loops
        .first()
        .map(|l| l.loop_type)
        .filter(|t| *t >= 2 && loop_unchanged);
    let first = match e.loop_mode {
        LoopMode::Off => None,
        LoopMode::Forward => Some(kept_type.unwrap_or(0)),
        LoopMode::PingPong => Some(kept_type.unwrap_or(1)),
    }
    .map(|loop_type| SmplLoop {
        cue_id: 0,
        loop_type,
        start: to_u32(e.loop_start - e.start),
        end: to_u32(e.loop_end - e.start),
        fraction: 0,
        play_count: 0,
    });
    let range = e.start..=e.end;
    let shift = to_u32(e.start);
    let extra = s
        .loops
        .iter()
        .skip(1)
        .filter(|l| range.contains(&(l.start as usize)) && range.contains(&(l.end as usize)))
        .map(|l| SmplLoop {
            start: l.start - shift,
            end: l.end - shift,
            ..*l
        });
    s.loops = first.into_iter().chain(extra).collect();
    s
}

/// The complete WAV file for `doc` edited by `e` (spec §6.5–6.6). `e` is clamped to the
/// document first, so any edit state is accepted.
pub fn build_wav_bytes(doc: &WavDocument, e: &EditState) -> Vec<u8> {
    let e = e.clone().clamped(doc.frames);
    let init = doc.initial_edit();
    let mut smpl = (doc.smpl.is_some() || tuning_or_loop_changed(&init, &e))
        .then(|| edited_smpl(doc, &init, &e).to_bytes());
    let mut data = Some(edited_data(doc, &e));
    let mut data_at = None;
    let mut smpl_seen = false;

    let mut chunks = Vec::with_capacity(doc.riff.chunks.len() + 2);
    for c in &doc.riff.chunks {
        let mut c = c.clone();
        match &c.id {
            b"cue " => continue,
            b"LIST" if list_type(&c) == Some(*b"adtl") => continue,
            // Only the first `data` chunk is the audio; later duplicates pass through.
            b"data" if data.is_some() => {
                c.data = data.take().unwrap_or_default();
                data_at = Some(chunks.len());
            }
            // The first `smpl` is replaced, or kept byte for byte when it was unreadable
            // and nothing needs a new one. Duplicates are dropped.
            b"smpl" if smpl_seen => continue,
            b"smpl" => {
                smpl_seen = true;
                if let Some(bytes) = smpl.take() {
                    c.data = bytes;
                }
            }
            b"inst" => sync_inst(&mut c.data, e.root, e.cents),
            b"acid" => sync_acid(&mut c.data, e.root),
            _ => {}
        }
        chunks.push(c);
    }
    if let Some(bytes) = smpl {
        let at = data_at.map_or(chunks.len(), |i| i + 1);
        chunks.insert(
            at,
            Chunk {
                id: *b"smpl",
                data: bytes,
            },
        );
    }
    info_set(&mut chunks, b"ISFT", &software_tag());
    write_riff(&RiffFile {
        chunks,
        warnings: Vec::new(),
    })
}

/// Whether two paths name the same file. Falls back to comparing the paths as given when
/// either cannot be resolved (e.g. it does not exist).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Writes `bytes` to a temporary file next to `target`, then renames it over `target`.
/// On any error the temporary file is removed and `target` is untouched.
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    // Keep the replaced file's permissions (the temporary file is created private).
    if let Ok(meta) = std::fs::metadata(target) {
        let perms = meta.permissions();
        if !perms.readonly() {
            std::fs::set_permissions(tmp.path(), perms)?;
        }
    }
    tmp.persist(target).map_err(|e| SaveError::Persist {
        path: target.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

/// Writes `doc` edited by `e` to `dest`.
///
/// `Folder` creates the folder if needed and refuses a folder that would put the file
/// over the original; a different file it replaces is backed up first.
/// `OverwriteWithBackup` first backs up the original. A failed backup aborts the save
/// with [`SaveError::Backup`] before anything is written. `ReplaceSaved` writes over
/// `doc.path` with no backup. The write itself is atomic.
pub fn save_wav(
    doc: &WavDocument,
    e: &EditState,
    dest: &Destination,
) -> Result<SaveOutcome, SaveError> {
    let bytes = build_wav_bytes(doc, e);
    let (target, backup) = match dest {
        Destination::Folder(dir) => {
            let name = doc
                .path
                .file_name()
                .ok_or_else(|| SaveError::NoFileName(doc.path.clone()))?;
            let target = dir.join(name);
            if same_file(&target, &doc.path) {
                return Err(SaveError::WouldOverwriteOriginal(target));
            }
            std::fs::create_dir_all(dir)?;
            let backup = match std::fs::symlink_metadata(&target) {
                Ok(_) => Some(make_backup(&target)?),
                Err(_) => None,
            };
            (target, backup)
        }
        Destination::OverwriteWithBackup => {
            let backup = make_backup(&doc.path)?;
            (doc.path.clone(), Some(backup))
        }
        Destination::ReplaceSaved => (doc.path.clone(), None),
    };
    write_atomic(&target, &bytes)?;
    Ok(SaveOutcome {
        written: target,
        backup,
    })
}

/// Backup name number `n` for `original`: `<name>.bak` for 0, else `<name>.<n>.bak`.
fn backup_candidate(original: &Path, n: u64) -> PathBuf {
    let mut name = original.file_name().unwrap_or_default().to_os_string();
    if n == 0 {
        name.push(".bak");
    } else {
        name.push(format!(".{n}.bak"));
    }
    original.with_file_name(name)
}

/// The first of `<name>.bak`, `<name>.1.bak`, `<name>.2.bak`, … that does not exist (a
/// dangling link counts as existing).
pub fn backup_path(original: &Path) -> PathBuf {
    (0..)
        .map(|n| backup_candidate(original, n))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| backup_candidate(original, 0))
}

/// Copies `original` to the first free numbered backup name and returns it. Each name is
/// created exclusively, so an existing file or link is never written through or replaced
/// (a name taken in the meantime moves on to the next number), and a partial copy is
/// removed.
fn make_backup(original: &Path) -> Result<PathBuf, SaveError> {
    let mut source = std::fs::File::open(original).map_err(|source| SaveError::Backup {
        path: backup_candidate(original, 0),
        source,
    })?;
    let mut n = 0;
    loop {
        let path = backup_candidate(original, n);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                n += 1;
                continue;
            }
            Err(source) => return Err(SaveError::Backup { path, source }),
        };
        let copied = std::io::copy(&mut source, &mut file).and_then(|_| file.sync_all());
        drop(file);
        return match copied {
            Ok(()) => {
                // Like `fs::copy`, keep the original's permissions (best effort).
                if let Ok(meta) = source.metadata() {
                    let _ = std::fs::set_permissions(&path, meta.permissions());
                }
                Ok(path)
            }
            Err(source) => {
                let _ = std::fs::remove_file(&path);
                Err(SaveError::Backup { path, source })
            }
        };
    }
}

/// The default save folder: `<source dir>/seamlime`.
pub fn default_folder(source: &Path) -> PathBuf {
    source
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("seamlime")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{load_wav, load_wav_bytes};
    use crate::edit::FadeShape;
    use crate::meta::info_get;
    use crate::riff::{find, parse_riff};
    use crate::testwav::{sine, wav_bytes};
    use crate::wav::SampleFormat;

    const RATE: u32 = 44100;
    const FRAMES: usize = 4000;
    const I16: SampleFormat = SampleFormat::Int { bits: 16 };

    fn chunk(id: &[u8; 4], data: Vec<u8>) -> Chunk {
        Chunk { id: *id, data }
    }

    /// A mono 16-bit file: `fmt `, `before`, `data`, `after`.
    fn file(before: Vec<Chunk>, after: Vec<Chunk>) -> Vec<u8> {
        let audio = vec![sine(441.0, RATE, FRAMES, 0.8)];
        let mut riff = parse_riff(&wav_bytes(&audio, RATE, I16, before)).unwrap();
        riff.chunks.extend(after);
        write_riff(&riff)
    }

    fn doc(bytes: &[u8]) -> WavDocument {
        load_wav_bytes(PathBuf::from("x.wav"), bytes).unwrap()
    }

    fn chunks(bytes: &[u8]) -> Vec<Chunk> {
        parse_riff(bytes).unwrap().chunks
    }

    fn ids(bytes: &[u8]) -> Vec<[u8; 4]> {
        chunks(bytes).iter().map(|c| c.id).collect()
    }

    fn data_of(bytes: &[u8]) -> Vec<u8> {
        let c = chunks(bytes);
        c[find(&c, b"data").unwrap()].data.clone()
    }

    fn smpl_of(bytes: &[u8]) -> Option<Smpl> {
        let c = chunks(bytes);
        find(&c, b"smpl").map(|i| Smpl::parse(&c[i].data).unwrap())
    }

    fn fwd(start: u32, end: u32) -> SmplLoop {
        SmplLoop {
            cue_id: 0,
            loop_type: 0,
            start,
            end,
            fraction: 0,
            play_count: 0,
        }
    }

    fn smpl_chunk(loops: Vec<SmplLoop>) -> Chunk {
        let s = Smpl {
            manufacturer: 0x47,
            product: 7,
            sample_period: sample_period(RATE),
            unity_note: 60,
            pitch_fraction: 0,
            smpte_format: 0,
            smpte_offset: 0,
            loops,
            sampler_data: vec![1, 2, 3, 4],
        };
        chunk(b"smpl", s.to_bytes())
    }

    fn info(pairs: &[(&[u8; 4], &str)]) -> Chunk {
        let mut c = Vec::new();
        for (k, v) in pairs {
            info_set(&mut c, k, v);
        }
        c.remove(0)
    }

    fn looped(d: &WavDocument, loop_start: usize, loop_end: usize) -> EditState {
        EditState {
            loop_mode: LoopMode::Forward,
            loop_start,
            loop_end,
            ..d.initial_edit()
        }
    }

    #[test]
    fn no_edit_roundtrip_is_identical_except_isft() {
        let tag = software_tag();
        let bytes = file(
            vec![
                smpl_chunk(vec![fwd(2532, 2915), fwd(100, 200)]),
                info(&[(b"INAM", "Lead"), (b"ISFT", &tag)]),
            ],
            vec![],
        );
        let d = doc(&bytes);
        assert_eq!(build_wav_bytes(&d, &d.initial_edit()), bytes);

        let dir = tempfile::tempdir().unwrap();
        let out = save_wav(
            &d,
            &d.initial_edit(),
            &Destination::Folder(dir.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(std::fs::read(&out.written).unwrap(), bytes);
    }

    #[test]
    fn unknown_chunks_preserved_in_order() {
        let bytes = file(
            vec![chunk(b"abcd", vec![1, 2, 3])],
            vec![chunk(b"wxyz", vec![9; 6])],
        );
        let d = doc(&bytes);
        let out = build_wav_bytes(&d, &d.initial_edit());
        assert_eq!(
            ids(&out),
            vec![*b"fmt ", *b"abcd", *b"data", *b"wxyz", *b"LIST"]
        );
        assert_eq!(chunks(&out)[..4], chunks(&bytes)[..]);
    }

    #[test]
    fn cue_and_adtl_dropped() {
        let mut adtl = b"adtl".to_vec();
        adtl.extend_from_slice(b"labl\x06\0\0\0\x01\0\0\0a\0");
        let bytes = file(
            vec![
                chunk(b"cue ", vec![0; 4]),
                chunk(b"LIST", adtl),
                info(&[(b"INAM", "Lead")]),
            ],
            vec![],
        );
        let d = doc(&bytes);
        assert!(d.has_markers);
        let out = build_wav_bytes(&d, &d.initial_edit());
        let c = chunks(&out);
        assert_eq!(find(&c, b"cue "), None);
        assert!(c.iter().all(|c| list_type(c) != Some(*b"adtl")));
        assert_eq!(info_get(&c, b"INAM").as_deref(), Some("Lead"));
    }

    #[test]
    fn smpl_inclusive_end_written() {
        let d = doc(&file(vec![], vec![]));
        let out = build_wav_bytes(&d, &looped(&d, 2532, 2915));
        let s = smpl_of(&out).unwrap();
        assert_eq!(s.loops, vec![fwd(2532, 2915)]);

        let pingpong = EditState {
            loop_mode: LoopMode::PingPong,
            ..looped(&d, 2532, 2915)
        };
        let s = smpl_of(&build_wav_bytes(&d, &pingpong)).unwrap();
        assert_eq!(s.loops[0].loop_type, 1);
        assert_eq!((s.loops[0].start, s.loops[0].end), (2532, 2915));
    }

    #[test]
    fn one_shot_keeps_zero_loop_smpl() {
        let d = doc(&file(vec![], vec![]));
        let e = EditState {
            root: 62,
            ..d.initial_edit()
        };
        let s = smpl_of(&build_wav_bytes(&d, &e)).unwrap();
        assert!(s.loops.is_empty());
        assert_eq!((s.unity_note, s.pitch_fraction), (62, 0));
        assert_eq!(s.sample_period, sample_period(RATE));
        assert_eq!((s.manufacturer, s.product), (0, 0));
        assert!(s.sampler_data.is_empty());
    }

    #[test]
    fn no_smpl_added_without_tuning_or_loop_change() {
        let d = doc(&file(vec![], vec![]));
        let e = EditState {
            start: 10,
            end: 3000,
            crossfade: 50,
            ..d.initial_edit()
        };
        assert_eq!(smpl_of(&build_wav_bytes(&d, &d.initial_edit())), None);
        assert_eq!(smpl_of(&build_wav_bytes(&d, &e)), None);
    }

    #[test]
    fn smpl_added_when_root_changed() {
        let d = doc(&file(vec![], vec![]));
        let e = EditState {
            root: 50,
            ..d.initial_edit()
        };
        let out = build_wav_bytes(&d, &e);
        assert_eq!(ids(&out), vec![*b"fmt ", *b"data", *b"smpl", *b"LIST"]);
        assert_eq!(smpl_of(&out).unwrap().unity_note, 50);
    }

    #[test]
    fn isft_replaced_inam_kept() {
        let bytes = file(
            vec![info(&[(b"INAM", "Lead"), (b"ISFT", "Other Tool 1.0")])],
            vec![],
        );
        let d = doc(&bytes);
        let c = chunks(&build_wav_bytes(&d, &d.initial_edit()));
        assert_eq!(info_get(&c, b"ISFT"), Some(software_tag()));
        assert_eq!(info_get(&c, b"INAM").as_deref(), Some("Lead"));
        assert_eq!(c.iter().filter(|c| &c.id == b"LIST").count(), 1);
    }

    #[test]
    fn lax_info_lists_lose_no_tags_on_save() {
        // INAM "Lead\0" is odd-sized and its pad byte is missing.
        let mut lax = b"INFOINAM\x05\0\0\0Lead\0".to_vec();
        lax.extend_from_slice(b"ICMT\x04\0\0\0abc\0");
        let d = doc(&file(vec![chunk(b"LIST", lax)], vec![]));
        let c = chunks(&build_wav_bytes(&d, &d.initial_edit()));
        assert_eq!(info_get(&c, b"INAM").as_deref(), Some("Lead"));
        assert_eq!(info_get(&c, b"ICMT").as_deref(), Some("abc"));
        assert_eq!(info_get(&c, b"ISFT"), Some(software_tag()));

        // A list that cannot be read to its end is written back byte for byte.
        let mut broken = b"INFOINAM\x04\0\0\0abc\0".to_vec();
        broken.extend_from_slice(&[1, 2, 3, 4, 0xff, 0xff, 0, 0, 9, 9]);
        let d = doc(&file(vec![chunk(b"LIST", broken.clone())], vec![]));
        let c = chunks(&build_wav_bytes(&d, &d.initial_edit()));
        assert_eq!(c[find(&c, b"LIST").unwrap()].data, broken);
    }

    #[test]
    fn head_trim_shifts_loop_offsets() {
        let bytes = file(
            vec![smpl_chunk(vec![
                fwd(2532, 2915),
                fwd(1500, 1600),
                fwd(500, 600),
            ])],
            vec![],
        );
        let d = doc(&bytes);
        let e = EditState {
            start: 1000,
            ..d.initial_edit()
        };
        let out = build_wav_bytes(&d, &e);
        // The extra loop before Start is dropped; the one inside is shifted.
        assert_eq!(
            smpl_of(&out).unwrap().loops,
            vec![fwd(1532, 1915), fwd(500, 600)]
        );
        assert_eq!(data_of(&out), data_of(&bytes)[1000 * 2..]);
    }

    #[test]
    fn trim_after_loop_end_padding() {
        let bytes = file(vec![smpl_chunk(vec![fwd(2532, 2915)])], vec![]);
        let d = doc(&bytes);
        let e = EditState {
            start: 1000,
            end: 2915 + 16,
            ..d.initial_edit()
        };
        let out = build_wav_bytes(&d, &e);
        let data = data_of(&out);
        assert_eq!(data.len() / 2, 2915 + 17 - 1000);
        assert_eq!(data, data_of(&bytes)[1000 * 2..(2915 + 17) * 2]);
        assert_eq!(doc(&out).frames, 2915 + 17 - 1000);
    }

    #[test]
    fn crossfade_reads_before_start_trim() {
        let bytes = file(vec![], vec![]);
        let d = doc(&bytes);
        let orig = data_of(&bytes);
        let loop_end = 2000;
        for start in [1000, 1005] {
            let e = EditState {
                start,
                crossfade: 10,
                fade_shape: FadeShape::Linear,
                ..looped(&d, 1010, loop_end)
            };
            let data = data_of(&build_wav_bytes(&d, &e));
            let at = (loop_end - start) * 2;
            assert_eq!(data[at..at + 2], orig[1009 * 2..1010 * 2], "start {start}");
            // The crossfade really changed the frames before loop_end.
            assert_ne!(
                data[at - 10..at],
                orig[(loop_end - 5) * 2..loop_end * 2],
                "start {start}"
            );
        }
    }

    #[test]
    fn bytes_outside_crossfade_untouched_32bit() {
        let audio = vec![
            sine(441.0, RATE, FRAMES, 0.8),
            sine(330.0, RATE, FRAMES, 0.5),
        ];
        let mut riff = parse_riff(&wav_bytes(
            &audio,
            RATE,
            SampleFormat::Int { bits: 32 },
            vec![],
        ))
        .unwrap();
        // Fill the low byte of every sample with noise so a decode/encode round trip
        // through f32 would change it.
        let data_idx = find(&riff.chunks, b"data").unwrap();
        let mut seed = 0x1234_5678u32;
        for sample in riff.chunks[data_idx].data.chunks_exact_mut(4) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            sample[0] = (seed >> 24) as u8;
        }
        let bytes = write_riff(&riff);
        let d = doc(&bytes);
        let e = EditState {
            crossfade: 100,
            ..looped(&d, 1000, 1999)
        };
        let orig = data_of(&bytes);
        let data = data_of(&build_wav_bytes(&d, &e));
        let stride = 8;
        let (from, to) = (1900 * stride, 2000 * stride);
        assert_eq!(data.len(), orig.len());
        assert_eq!(data[..from], orig[..from]);
        assert_eq!(data[to..], orig[to..]);
        assert_ne!(data[from..to], orig[from..to]);
    }

    #[test]
    fn inst_synced_when_present() {
        let mut acid = vec![0xAA; 24];
        acid[4..6].copy_from_slice(&60u16.to_le_bytes());
        let bytes = file(
            vec![
                chunk(b"inst", vec![60, 0, 0, 0, 127, 1, 127]),
                chunk(b"acid", acid),
            ],
            vec![],
        );
        let d = doc(&bytes);
        let e = EditState {
            root: 48,
            cents: 12.0,
            ..d.initial_edit()
        };
        let c = chunks(&build_wav_bytes(&d, &e));
        assert_eq!(
            c[find(&c, b"inst").unwrap()].data,
            vec![48, (-12i8) as u8, 0, 0, 127, 1, 127]
        );
        let acid = &c[find(&c, b"acid").unwrap()].data;
        assert_eq!(acid[4..6], 48u16.to_le_bytes());
        assert_eq!(acid[..4], [0xAA; 4]);
    }

    #[test]
    fn overwrite_makes_numbered_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        let original = file(vec![], vec![]);
        std::fs::write(&path, &original).unwrap();
        let old_backup = dir.path().join("x.wav.bak");
        std::fs::write(&old_backup, b"older backup").unwrap();

        let d = load_wav(&path).unwrap();
        let e = EditState {
            root: 50,
            ..d.initial_edit()
        };
        let out = save_wav(&d, &e, &Destination::OverwriteWithBackup).unwrap();
        let new_backup = dir.path().join("x.wav.1.bak");
        assert_eq!(out.written, path);
        assert_eq!(out.backup.as_deref(), Some(new_backup.as_path()));
        assert_eq!(std::fs::read(&new_backup).unwrap(), original);
        assert_eq!(std::fs::read(&old_backup).unwrap(), b"older backup");
        let written = std::fs::read(&path).unwrap();
        assert_ne!(written, original);
        assert_eq!(written, build_wav_bytes(&d, &e));
        assert_eq!(backup_path(&path), dir.path().join("x.wav.2.bak"));
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn failed_backup_aborts_the_save() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = file(vec![], vec![]);
        let e = |d: &WavDocument| EditState {
            root: 50,
            ..d.initial_edit()
        };

        // The original is gone: nothing to back up, so nothing is written.
        let gone = load_wav_bytes(dir.path().join("gone.wav"), &bytes).unwrap();
        let err = save_wav(&gone, &e(&gone), &Destination::OverwriteWithBackup).unwrap_err();
        assert!(matches!(err, SaveError::Backup { .. }), "{err}");
        assert!(
            names_in(dir.path()).is_empty(),
            "no file, no partial backup"
        );

        // The "original" is a folder: it cannot be copied, and no partial backup is left.
        let folder = dir.path().join("folder.wav");
        std::fs::create_dir(&folder).unwrap();
        let d = load_wav_bytes(folder.clone(), &bytes).unwrap();
        let err = save_wav(&d, &e(&d), &Destination::OverwriteWithBackup).unwrap_err();
        assert!(matches!(err, SaveError::Backup { .. }), "{err}");
        assert_eq!(names_in(dir.path()), vec!["folder.wav"]);
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
    }

    #[test]
    fn folder_save_backs_up_a_different_file_it_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        let original = file(vec![], vec![]);
        std::fs::write(&path, &original).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("x.wav"), b"unrelated").unwrap();

        let d = load_wav(&path).unwrap();
        let e = EditState {
            root: 50,
            ..d.initial_edit()
        };
        let saved = save_wav(&d, &e, &Destination::Folder(out.clone())).unwrap();
        assert_eq!(saved.written, out.join("x.wav"));
        assert_eq!(saved.backup, Some(out.join("x.wav.bak")));
        assert_eq!(std::fs::read(out.join("x.wav.bak")).unwrap(), b"unrelated");
        assert_eq!(
            std::fs::read(out.join("x.wav")).unwrap(),
            build_wav_bytes(&d, &e)
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn backup_never_follows_a_dangling_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        let original = file(vec![], vec![]);
        std::fs::write(&path, &original).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("x.wav.bak")).unwrap();

        let d = load_wav(&path).unwrap();
        let out = save_wav(&d, &d.initial_edit(), &Destination::OverwriteWithBackup).unwrap();
        assert_eq!(out.backup, Some(dir.path().join("x.wav.1.bak")));
        assert!(!elsewhere.exists(), "the link target was not written");
        assert_eq!(
            std::fs::read(dir.path().join("x.wav.1.bak")).unwrap(),
            original
        );
    }

    #[test]
    fn replace_saved_writes_doc_path_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        std::fs::write(&path, file(vec![], vec![])).unwrap();

        let d = load_wav(&path).unwrap();
        let e = EditState {
            root: 50,
            ..d.initial_edit()
        };
        let out = save_wav(&d, &e, &Destination::ReplaceSaved).unwrap();
        assert_eq!(out.written, path);
        assert_eq!(out.backup, None);
        assert_eq!(std::fs::read(&path).unwrap(), build_wav_bytes(&d, &e));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["x.wav"], "no backup, no temporary file left");
    }

    #[test]
    fn save_creates_destination_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        let original = file(vec![], vec![]);
        std::fs::write(&path, &original).unwrap();
        let dest = dir.path().join("seamlime").join("nested");
        assert!(!dest.exists());

        let d = load_wav(&path).unwrap();
        let out = save_wav(&d, &d.initial_edit(), &Destination::Folder(dest.clone())).unwrap();
        assert_eq!(out.written, dest.join("x.wav"));
        assert_eq!(out.backup, None);
        assert_eq!(load_wav(&out.written).unwrap().frames, FRAMES);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn failed_save_leaves_original_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        let original = file(vec![], vec![]);
        std::fs::write(&path, &original).unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"a file, not a folder").unwrap();

        let d = load_wav(&path).unwrap();
        let e = EditState {
            root: 50,
            ..d.initial_edit()
        };
        assert!(save_wav(&d, &e, &Destination::Folder(blocker.clone())).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["blocker", "x.wav"]);
    }

    #[test]
    fn folder_mode_refuses_the_source_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        let original = file(vec![], vec![]);
        std::fs::write(&path, &original).unwrap();

        let d = load_wav(&path).unwrap();
        let e = EditState {
            root: 50,
            ..d.initial_edit()
        };
        let same = dir.path().join(".");
        let err = save_wav(&d, &e, &Destination::Folder(same)).unwrap_err();
        assert!(matches!(err, SaveError::WouldOverwriteOriginal(_)), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn unicode_and_space_file_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Ström sample 1.wav");
        std::fs::write(&path, file(vec![], vec![])).unwrap();

        let d = load_wav(&path).unwrap();
        let e = looped(&d, 2532, 2915);
        let out = save_wav(&d, &e, &Destination::Folder(default_folder(&path))).unwrap();
        assert_eq!(
            out.written,
            dir.path().join("seamlime").join("Ström sample 1.wav")
        );
        let reloaded = load_wav(&out.written).unwrap();
        assert_eq!(reloaded.frames, FRAMES);
        assert_eq!(reloaded.initial_edit().loop_end, 2915);
    }

    #[test]
    fn backup_and_default_folder_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a b.wav");
        assert_eq!(backup_path(&path), dir.path().join("a b.wav.bak"));
        std::fs::write(dir.path().join("a b.wav.bak"), b"").unwrap();
        std::fs::write(dir.path().join("a b.wav.1.bak"), b"").unwrap();
        assert_eq!(backup_path(&path), dir.path().join("a b.wav.2.bak"));
        assert_eq!(default_folder(&path), dir.path().join("seamlime"));
    }

    #[test]
    fn unreadable_smpl_kept_unless_a_new_one_is_needed() {
        let tag = software_tag();
        let bytes = file(
            vec![chunk(b"smpl", vec![7; 10]), info(&[(b"ISFT", &tag)])],
            vec![],
        );
        let d = doc(&bytes);
        assert!(d.smpl.is_none());
        assert_eq!(build_wav_bytes(&d, &d.initial_edit()), bytes);

        let e = EditState {
            root: 50,
            ..d.initial_edit()
        };
        let out = build_wav_bytes(&d, &e);
        assert_eq!(ids(&out), vec![*b"fmt ", *b"smpl", *b"LIST", *b"data"]);
        assert_eq!(smpl_of(&out).unwrap().unity_note, 50);
    }

    #[test]
    fn backward_and_unknown_loop_types_kept_until_loop_edited() {
        let tag = software_tag();
        for original_type in [2, 7] {
            let bytes = file(
                vec![
                    smpl_chunk(vec![SmplLoop {
                        loop_type: original_type,
                        ..fwd(2532, 2915)
                    }]),
                    info(&[(b"ISFT", &tag)]),
                ],
                vec![],
            );
            let d = doc(&bytes);
            let init = d.initial_edit();
            assert_eq!(build_wav_bytes(&d, &init), bytes, "type {original_type}");

            let trimmed = EditState {
                start: 1000,
                ..init.clone()
            };
            let s = smpl_of(&build_wav_bytes(&d, &trimmed)).unwrap();
            let want = SmplLoop {
                loop_type: original_type,
                ..fwd(1532, 1915)
            };
            assert_eq!(s.loops, vec![want]);

            let moved = EditState {
                loop_start: 2600,
                ..init.clone()
            };
            let s = smpl_of(&build_wav_bytes(&d, &moved)).unwrap();
            assert_eq!(s.loops, vec![fwd(2600, 2915)]);

            let pingpong = EditState {
                loop_mode: LoopMode::PingPong,
                ..init.clone()
            };
            let s = smpl_of(&build_wav_bytes(&d, &pingpong)).unwrap();
            assert_eq!(s.loops[0].loop_type, 1);

            let off = EditState {
                loop_mode: LoopMode::Off,
                ..init
            };
            assert!(
                smpl_of(&build_wav_bytes(&d, &off))
                    .unwrap()
                    .loops
                    .is_empty()
            );
        }
    }

    #[test]
    fn wild_edit_states_never_panic() {
        let d = doc(&file(vec![smpl_chunk(vec![fwd(2532, 2915)])], vec![]));
        let wild = [
            EditState {
                start: 5000,
                end: 10,
                ..d.initial_edit()
            },
            EditState {
                loop_start: 9000,
                loop_end: 3,
                crossfade: usize::MAX,
                cents: f32::NAN,
                ..looped(&d, 0, 0)
            },
            EditState {
                start: usize::MAX,
                end: usize::MAX,
                loop_mode: LoopMode::PingPong,
                crossfade: 100_000,
                ..d.initial_edit()
            },
        ];
        for e in &wild {
            assert!(load_wav_bytes(PathBuf::from("y.wav"), &build_wav_bytes(&d, e)).is_ok());
        }

        let empty = wav_bytes(&[Vec::new()], RATE, I16, vec![]);
        let d = doc(&empty);
        assert_eq!(d.frames, 0);
        for e in [d.initial_edit(), looped(&d, 5, 50)] {
            let out = build_wav_bytes(&d, &e);
            assert!(data_of(&out).is_empty());
        }
    }
}
