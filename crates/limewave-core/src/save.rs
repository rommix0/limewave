//! Saving: builds the edited WAV bytes (crossfade, trim, `smpl`, `ISFT`) and writes them
//! atomically, with numbered backups when overwriting.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cue::remap_markers;
use crate::document::WavDocument;
use crate::edit::{EditState, LoopMode};
use crate::effects::{AppliedEffect, Op, map_frame, process};
use crate::error::SaveError;
use crate::meta::{info_set, sync_acid, sync_inst};
use crate::render::crossfade_region;
use crate::riff::{Chunk, RiffFile, find, write_riff};
use crate::smpl::{Smpl, SmplLoop, sample_period, tuning_to_smpl};
use crate::software_tag;
use crate::wav::{WavFormat, encode_frames, fmt_with_channels, frame_stride, parse_fmt};

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

/// The inclusive frame ranges (sorted, merged) that effects or the crossfade change in a file
/// of `frames` frames. A channel tool changes every frame whatever its stored range.
fn changed_ranges(
    effects: &[AppliedEffect],
    crossfade: Option<(usize, usize)>,
    frames: usize,
) -> Vec<(usize, usize)> {
    let Some(last_frame) = frames.checked_sub(1) else {
        return Vec::new();
    };
    let mut ranges: Vec<(usize, usize)> = effects
        .iter()
        .map(|a| match a.op {
            Op::StereoToMono | Op::SwapChannels | Op::MonoToStereo => (0, last_frame),
            _ => (a.first, a.last.min(last_frame)),
        })
        .chain(crossfade)
        .filter(|(first, last)| first <= last)
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (first, last) in ranges {
        match merged.last_mut() {
            // Overlapping or touching the previous range.
            Some(prev) if first <= prev.1.saturating_add(1) => prev.1 = prev.1.max(last),
            _ => merged.push((first, last)),
        }
    }
    merged
}

/// Applies the loop crossfade to `audio` in place and returns the frames it replaced, if any.
fn crossfade_in_place(audio: &mut [Vec<f32>], e: &EditState) -> Option<(usize, usize)> {
    let (first, replacement) = crossfade_region(audio, e)?;
    let n = replacement.first().map_or(0, Vec::len);
    for (ch, rep) in audio.iter_mut().zip(&replacement) {
        ch[first..first + n].copy_from_slice(rep);
    }
    (n > 0).then(|| (first, first + n - 1))
}

/// The `data` bytes for `audio`, the effects-processed audio of `doc`, with the crossfade
/// applied and trimmed to frames `e.start..=e.end`. `e` must be clamped.
///
/// While the channel count is unchanged, the original bytes are kept and only the frames inside
/// an effect's range or the crossfade region are re-encoded. When it changed, all frames are
/// encoded with `format`, the new layout.
fn edited_data(
    doc: &WavDocument,
    e: &EditState,
    mut audio: Vec<Vec<f32>>,
    format: &WavFormat,
) -> Vec<u8> {
    if doc.frames == 0 {
        return Vec::new();
    }
    let crossfaded = crossfade_in_place(&mut audio, e);
    if audio.len() != usize::from(doc.format.channels) {
        let trimmed: Vec<Vec<f32>> = audio
            .iter()
            .map(|ch| ch[e.start.min(ch.len())..(e.end + 1).min(ch.len())].to_vec())
            .collect();
        return encode_frames(&trimmed, format);
    }

    let stride = frame_stride(format);
    let mut data = doc.data_chunk().to_vec();
    for (first, last) in changed_ranges(&e.effects, crossfaded, doc.frames) {
        let frames: Vec<Vec<f32>> = audio.iter().map(|ch| ch[first..=last].to_vec()).collect();
        let encoded = encode_frames(&frames, format);
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
/// Loop 0 is type 0 (forward), 1 (ping-pong) or 2 (backward), except that an original
/// unknown type (3 and up, shown as forward) is kept while the loop mode and points
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
        .filter(|t| *t >= 3 && loop_unchanged);
    let first = match e.loop_mode {
        LoopMode::Off => None,
        LoopMode::Forward => Some(kept_type.unwrap_or(0)),
        LoopMode::PingPong => Some(kept_type.unwrap_or(1)),
        LoopMode::Backward => Some(kept_type.unwrap_or(2)),
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
        .map(|l| {
            let (start, end) = (
                map_frame(&e.effects, l.start as usize),
                map_frame(&e.effects, l.end as usize),
            );
            // A mirroring reverse swaps the ends; an ordered loop stays ordered.
            if l.start <= l.end && start > end {
                (end, start, l)
            } else {
                (start, end, l)
            }
        })
        .filter(|(start, end, _)| range.contains(start) && range.contains(end))
        .map(|(start, end, l)| SmplLoop {
            start: to_u32(start) - shift,
            end: to_u32(end) - shift,
            ..*l
        });
    s.loops = first.into_iter().chain(extra).collect();
    s
}

/// Whether the original `smpl` chunk can be written back as it is: the loop mode, points, root
/// and cents are those of the file, nothing moves the sample numbers (no head trim, no
/// marker-mirroring reverse), and every extra loop still lies inside the kept frames.
fn smpl_unchanged(doc: &WavDocument, init: &EditState, e: &EditState) -> bool {
    let Some(smpl) = &doc.smpl else {
        return false;
    };
    (e.loop_mode, e.loop_start, e.loop_end, e.root, e.cents)
        == (
            init.loop_mode,
            init.loop_start,
            init.loop_end,
            init.root,
            init.cents,
        )
        && e.start == 0
        && !e.effects.iter().any(|a| {
            matches!(
                a.op,
                Op::Reverse {
                    mirror_markers: true
                }
            )
        })
        && smpl
            .loops
            .iter()
            .skip(1)
            .all(|l| l.start as usize <= e.end && l.end as usize <= e.end)
}

/// The complete WAV file for `doc` edited by `e` (spec §6.5–6.6). `e` is clamped to the
/// document first, so any edit state is accepted.
pub fn build_wav_bytes(doc: &WavDocument, e: &EditState) -> Vec<u8> {
    let e = e.clone().clamped(doc.frames);
    let init = doc.initial_edit();
    let mut smpl = (!smpl_unchanged(doc, &init, &e)
        && (doc.smpl.is_some() || tuning_or_loop_changed(&init, &e)))
    .then(|| edited_smpl(doc, &init, &e).to_bytes());
    let audio = process(&doc.audio, &e.effects);
    // `fmt ` is rewritten only when the effects changed the channel count.
    let mut new_fmt = u16::try_from(audio.len())
        .ok()
        .filter(|&n| n != doc.format.channels)
        .zip(find(&doc.riff.chunks, b"fmt "))
        .map(|(n, i)| fmt_with_channels(&doc.riff.chunks[i].data, n));
    let format = new_fmt
        .as_deref()
        .and_then(|bytes| parse_fmt(bytes).ok())
        .unwrap_or(doc.format);
    let mut data = Some(edited_data(doc, &e, audio, &format));
    // With no trim and no marker-mirroring reverse, no frame moves: cue and `fact` chunks are
    // written back as they are, so a save with no edits stays byte for byte.
    let frames_move = doc.frames != 0
        && (e.start != 0
            || e.end + 1 != doc.frames
            || e.effects.iter().any(|a| {
                matches!(
                    a.op,
                    Op::Reverse {
                        mirror_markers: true
                    }
                )
            }));
    let tuning_changed = (e.root, e.cents) != (init.root, init.cents);
    let fact_frames = u32::try_from(e.end - e.start + 1)
        .unwrap_or(u32::MAX)
        .to_le_bytes();
    let mut data_at = None;
    let mut smpl_seen = false;

    let mut chunks = Vec::with_capacity(doc.riff.chunks.len() + 2);
    for c in &doc.riff.chunks {
        let mut c = c.clone();
        match &c.id {
            // Only the first `fmt ` (the one in use) is rewritten, and only for a new layout.
            b"fmt " => {
                if let Some(bytes) = new_fmt.take() {
                    c.data = bytes;
                }
            }
            // The frame count follows the saved length.
            b"fact" if frames_move && c.data.len() >= 4 => {
                c.data[..4].copy_from_slice(&fact_frames)
            }
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
            // `inst` and `acid` follow a tuning change; otherwise they stay as they are.
            b"inst" if tuning_changed => sync_inst(&mut c.data, e.root, e.cents),
            b"acid" if tuning_changed => sync_acid(&mut c.data, e.root),
            _ => {}
        }
        chunks.push(c);
    }
    // Cue positions follow the trim and the reverses. An unreadable chunk stays as it is;
    // loading has already warned.
    if frames_move {
        let _ = remap_markers(&mut chunks, &|f| map_frame(&e.effects, f), e.start, e.end);
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
/// The result takes the permissions of the file it replaces, or of `source` when `target`
/// is new. On any error the temporary file is removed and `target` is untouched.
fn write_atomic(target: &Path, source: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    // The temporary file is created private (owner-only on Unix).
    if let Ok(meta) = std::fs::metadata(target).or_else(|_| std::fs::metadata(source)) {
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
    write_atomic(&target, &doc.path, &bytes)?;
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

/// The default save folder: `<source dir>/limewave`.
pub fn default_folder(source: &Path) -> PathBuf {
    source
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("limewave")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cue::{CuePoint, cue_bytes, parse_cue};
    use crate::document::{load_wav, load_wav_bytes};
    use crate::edit::FadeShape;
    use crate::effects::{AppliedEffect, Effect, Op, mirror_edit, plan_effect, process};
    use crate::meta::info_get;
    use crate::render::render;
    use crate::riff::{list_type, parse_riff};
    use crate::testwav::{sine, wav_bytes};
    use crate::wav::{SampleFormat, decode, parse_fmt};

    const RATE: u32 = 44100;
    const FRAMES: usize = 4000;
    const I16: SampleFormat = SampleFormat::Int { bits: 16 };

    fn chunk(id: &[u8; 4], data: Vec<u8>) -> Chunk {
        Chunk { id: *id, data }
    }

    fn cue_point(id: u32, at: u32) -> CuePoint {
        CuePoint {
            id,
            position: at,
            chunk_id: *b"data",
            chunk_start: 0,
            block_start: 0,
            sample_offset: at,
        }
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
    fn cue_and_adtl_kept() {
        let mut adtl = b"adtl".to_vec();
        adtl.extend_from_slice(b"labl\x06\0\0\0\x01\0\0\0a\0");
        let cue = cue_bytes(&[cue_point(1, 500)]);
        let bytes = file(
            vec![
                chunk(b"cue ", cue.clone()),
                chunk(b"LIST", adtl.clone()),
                info(&[(b"INAM", "Lead")]),
            ],
            vec![],
        );
        let d = doc(&bytes);
        let out = build_wav_bytes(&d, &d.initial_edit());
        let c = chunks(&out);
        assert_eq!(c[find(&c, b"cue ").unwrap()].data, cue);
        let list = c.iter().find(|c| list_type(c) == Some(*b"adtl")).unwrap();
        assert_eq!(list.data, adtl);
        assert_eq!(info_get(&c, b"INAM").as_deref(), Some("Lead"));

        // A head trim moves the cue; a reverse of the whole range mirrors it.
        let trimmed = EditState {
            start: 100,
            ..d.initial_edit()
        };
        let c = chunks(&build_wav_bytes(&d, &trimmed));
        let cues = parse_cue(&c[find(&c, b"cue ").unwrap()].data).unwrap();
        assert_eq!(cues[0].position, 400);

        let e = d.initial_edit();
        let a = plan_effect(Effect::Reverse, &d.audio, &e, None).unwrap();
        let mut e = mirror_edit(&e, a.first, a.last);
        e.effects.push(a);
        let c = chunks(&build_wav_bytes(&d, &e));
        let cues = parse_cue(&c[find(&c, b"cue ").unwrap()].data).unwrap();
        assert_eq!(cues[0].position, (FRAMES - 1 - 500) as u32);
    }

    #[test]
    fn unreadable_cue_chunk_is_saved_unchanged() {
        let bad = vec![9, 0, 0, 0, 1, 2, 3];
        let d = doc(&file(vec![chunk(b"cue ", bad.clone())], vec![]));
        let e = EditState {
            start: 100,
            ..d.initial_edit()
        };
        let c = chunks(&build_wav_bytes(&d, &e));
        assert_eq!(c[find(&c, b"cue ").unwrap()].data, bad);
    }

    #[test]
    fn fact_follows_the_trim() {
        let fact = chunk(b"fact", 1000u32.to_le_bytes().to_vec());
        let audio = vec![sine(441.0, RATE, 1000, 0.8)];
        let d = doc(&wav_bytes(&audio, RATE, I16, vec![fact]));
        let fact_of = |e: &EditState| {
            let c = chunks(&build_wav_bytes(&d, e));
            u32_at(&c[find(&c, b"fact").unwrap()].data, 0)
        };
        assert_eq!(fact_of(&d.initial_edit()), 1000);
        let e = EditState {
            start: 100,
            end: 899,
            ..d.initial_edit()
        };
        assert_eq!(fact_of(&e), 800);

        // A fact chunk too short to hold a count is left alone.
        let short = doc(&wav_bytes(
            &audio,
            RATE,
            I16,
            vec![chunk(b"fact", vec![1, 2])],
        ));
        let c = chunks(&build_wav_bytes(&short, &e));
        assert_eq!(c[find(&c, b"fact").unwrap()].data, vec![1, 2]);
    }

    #[test]
    fn no_edit_round_trip_is_byte_identical() {
        let tag = software_tag();
        let smpl = Smpl {
            manufacturer: 0x47,
            product: 7,
            sample_period: 12345,
            unity_note: 60,
            pitch_fraction: 0,
            smpte_format: 0,
            smpte_offset: 0,
            loops: vec![SmplLoop {
                cue_id: 7,
                ..fwd(1000, 1999)
            }],
            sampler_data: vec![1, 2, 3, 4],
        };
        let mut adtl = b"adtl".to_vec();
        adtl.extend_from_slice(b"labl\x06\0\0\0\x01\0\0\0a\0");
        // Cue 2 is a range that overruns the audio (3990..4040 of 4000 frames).
        let mut ltxt = vec![2, 0, 0, 0, 50, 0, 0, 0];
        ltxt.extend_from_slice(b"rgn \0\0\0\0\0\0\0\0");
        adtl.extend_from_slice(&[b'l', b't', b'x', b't', 20, 0, 0, 0]);
        adtl.extend_from_slice(&ltxt);
        let cues = [
            cue_point(1, 600),
            cue_point(2, 3990),
            // An end-of-file marker, and one well past the audio.
            cue_point(3, FRAMES as u32),
            cue_point(4, 9000),
        ];
        let bytes = file(
            vec![
                // The count disagrees with the real length: a no-edit save leaves it.
                chunk(b"fact", 1234u32.to_le_bytes().to_vec()),
                chunk(b"smpl", smpl.to_bytes()),
                chunk(b"cue ", cue_bytes(&cues)),
                chunk(b"LIST", adtl),
                chunk(b"abcd", vec![1, 2, 3]),
                info(&[(b"INAM", "Lead"), (b"ISFT", &tag)]),
            ],
            vec![],
        );
        let d = doc(&bytes);
        assert_eq!(build_wav_bytes(&d, &d.initial_edit()), bytes);
    }

    #[test]
    fn end_of_file_cue_follows_a_trim() {
        let eof = FRAMES as u32;
        let bytes = file(
            vec![chunk(
                b"cue ",
                cue_bytes(&[cue_point(1, 500), cue_point(2, eof)]),
            )],
            vec![],
        );
        let d = doc(&bytes);
        let positions = |e: &EditState| {
            let c = chunks(&build_wav_bytes(&d, e));
            let cues = parse_cue(&c[find(&c, b"cue ").unwrap()].data).unwrap();
            cues.iter().map(|p| (p.id, p.position)).collect::<Vec<_>>()
        };
        // A head trim moves the end-of-file cue to the new length.
        let head = EditState {
            start: 100,
            ..d.initial_edit()
        };
        assert_eq!(positions(&head), vec![(1, 400), (2, eof - 100)]);
        // A tail trim ending just before a cue keeps it as the new end-of-file marker.
        let tail = EditState {
            end: 2999,
            ..d.initial_edit()
        };
        assert_eq!(positions(&tail), vec![(1, 500)]);
        let cue_at_end = file(
            vec![chunk(b"cue ", cue_bytes(&[cue_point(1, 3000)]))],
            vec![],
        );
        let d2 = doc(&cue_at_end);
        let c = chunks(&build_wav_bytes(&d2, &tail));
        let cues = parse_cue(&c[find(&c, b"cue ").unwrap()].data).unwrap();
        assert_eq!(cues[0].position, 3000);
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

    /// An `acid` chunk body with `root` at byte 4 and 0xAA elsewhere.
    fn acid_chunk(root: u16) -> Chunk {
        let mut acid = vec![0xAA; 24];
        acid[4..6].copy_from_slice(&root.to_le_bytes());
        chunk(b"acid", acid)
    }

    #[test]
    fn no_edit_save_keeps_the_acid_root_of_a_file_without_smpl() {
        let tag = software_tag();
        let bytes = file(vec![acid_chunk(57), info(&[(b"ISFT", &tag)])], vec![]);
        let d = doc(&bytes);
        let init = d.initial_edit();
        assert_eq!((init.root, init.cents), (57, 0.0));
        assert_eq!(build_wav_bytes(&d, &init), bytes);
    }

    #[test]
    fn no_edit_save_keeps_the_inst_root_and_tune_of_a_file_without_smpl() {
        let tag = software_tag();
        let bytes = file(
            vec![
                chunk(b"inst", vec![48, (-12i8) as u8, 0, 0, 127, 1, 127]),
                // `inst` wins over `acid`.
                acid_chunk(50),
                info(&[(b"ISFT", &tag)]),
            ],
            vec![],
        );
        let d = doc(&bytes);
        let init = d.initial_edit();
        assert_eq!((init.root, init.cents), (48, 12.0));
        assert_eq!(build_wav_bytes(&d, &init), bytes);
    }

    #[test]
    fn no_edit_save_leaves_an_inst_that_disagrees_with_smpl() {
        let tag = software_tag();
        let bytes = file(
            vec![
                smpl_chunk(vec![fwd(2532, 2915)]),
                chunk(b"inst", vec![62, 0, 0, 0, 127, 1, 127]),
                acid_chunk(64),
                info(&[(b"ISFT", &tag)]),
            ],
            vec![],
        );
        let d = doc(&bytes);
        assert_eq!(d.initial_edit().root, 60, "smpl wins");
        assert_eq!(build_wav_bytes(&d, &d.initial_edit()), bytes);
    }

    #[test]
    fn a_tuning_change_still_syncs_inst_and_acid() {
        let bytes = file(
            vec![
                chunk(b"inst", vec![48, 0, 0, 0, 127, 1, 127]),
                acid_chunk(48),
            ],
            vec![],
        );
        let d = doc(&bytes);
        let inst_acid = |e: &EditState| {
            let c = chunks(&build_wav_bytes(&d, e));
            (
                c[find(&c, b"inst").unwrap()].data[..2].to_vec(),
                c[find(&c, b"acid").unwrap()].data[4..6].to_vec(),
            )
        };
        let init = d.initial_edit();
        let tuned = EditState {
            cents: 10.0,
            ..init.clone()
        };
        assert_eq!(
            inst_acid(&tuned),
            (vec![48, (-10i8) as u8], 48u16.to_le_bytes().to_vec())
        );
        let rooted = EditState { root: 50, ..init };
        assert_eq!(
            inst_acid(&rooted),
            (vec![50, 0], 50u16.to_le_bytes().to_vec())
        );
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

    #[cfg(unix)]
    #[test]
    fn new_files_take_the_source_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        std::fs::write(&path, file(vec![], vec![])).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let d = load_wav(&path).unwrap();
        let out = save_wav(
            &d,
            &d.initial_edit(),
            &Destination::Folder(dir.path().join("o")),
        )
        .unwrap();
        let mode = std::fs::metadata(&out.written)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
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
        let dest = dir.path().join("limewave").join("nested");
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
            dir.path().join("limewave").join("Ström sample 1.wav")
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
        assert_eq!(default_folder(&path), dir.path().join("limewave"));
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
    fn unknown_loop_type_kept_until_loop_edited() {
        let tag = software_tag();
        let bytes = file(
            vec![
                smpl_chunk(vec![SmplLoop {
                    loop_type: 5,
                    ..fwd(2532, 2915)
                }]),
                info(&[(b"ISFT", &tag)]),
            ],
            vec![],
        );
        let d = doc(&bytes);
        let init = d.initial_edit();
        assert_eq!(build_wav_bytes(&d, &init), bytes);

        let trimmed = EditState {
            start: 1000,
            ..init.clone()
        };
        let s = smpl_of(&build_wav_bytes(&d, &trimmed)).unwrap();
        let want = SmplLoop {
            loop_type: 5,
            ..fwd(1532, 1915)
        };
        assert_eq!(s.loops, vec![want]);

        let moved = EditState {
            loop_end: 2900,
            ..init.clone()
        };
        let s = smpl_of(&build_wav_bytes(&d, &moved)).unwrap();
        assert_eq!(s.loops, vec![fwd(2532, 2900)]);

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

    #[test]
    fn backward_loop_written_as_type_2() {
        let d = doc(&file(vec![smpl_chunk(vec![fwd(100, 500)])], vec![]));
        let init = d.initial_edit();
        assert_eq!(init.loop_mode, LoopMode::Forward);
        let e = EditState {
            loop_mode: LoopMode::Backward,
            ..init
        };
        let s = smpl_of(&build_wav_bytes(&d, &e)).unwrap();
        assert_eq!(s.loops[0].loop_type, 2);
        assert_eq!((s.loops[0].start, s.loops[0].end), (100, 500));
    }

    /// `e` with `effect` planned on the audio of `d` and pushed onto its effects.
    fn with_effect(
        d: &WavDocument,
        mut e: EditState,
        effect: Effect,
        selection: Option<(usize, usize)>,
    ) -> EditState {
        let a = plan_effect(effect, &d.audio, &e, selection).unwrap();
        e.effects.push(a);
        e
    }

    /// A 32-bit file whose samples are noise in every byte (below half scale).
    fn noisy_32bit(channels: &[Vec<f32>]) -> Vec<u8> {
        let mut riff = parse_riff(&wav_bytes(
            channels,
            RATE,
            SampleFormat::Int { bits: 32 },
            vec![],
        ))
        .unwrap();
        let data_idx = find(&riff.chunks, b"data").unwrap();
        let mut seed = 0x1234_5678u32;
        for sample in riff.chunks[data_idx].data.chunks_exact_mut(4) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            sample.copy_from_slice(&(seed as i32 / 2).to_le_bytes());
        }
        write_riff(&riff)
    }

    fn u32_at(d: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(d[at..at + 4].try_into().unwrap())
    }

    fn fmt_of(bytes: &[u8]) -> Vec<u8> {
        let c = chunks(bytes);
        c[find(&c, b"fmt ").unwrap()].data.clone()
    }

    fn stereo_file() -> Vec<u8> {
        let audio = vec![
            sine(441.0, RATE, FRAMES, 0.8),
            sine(330.0, RATE, FRAMES, 0.4),
        ];
        wav_bytes(&audio, RATE, I16, vec![])
    }

    const LSB: f32 = 1.0 / 32768.0 + 1e-6;

    #[test]
    fn effects_are_written_and_untouched_frames_keep_their_bytes() {
        let bytes = noisy_32bit(&[
            sine(441.0, RATE, FRAMES, 0.8),
            sine(330.0, RATE, FRAMES, 0.5),
        ]);
        let d = doc(&bytes);
        let e = with_effect(
            &d,
            d.initial_edit(),
            Effect::Gain { db: -6.0 },
            Some((100, 199)),
        );
        let orig = data_of(&bytes);
        let data = data_of(&build_wav_bytes(&d, &e));
        let stride = 8;
        let (from, to) = (100 * stride, 200 * stride);
        assert_eq!(data.len(), orig.len());
        assert_eq!(data[..from], orig[..from]);
        assert_eq!(data[to..], orig[to..]);

        let gain = 10f32.powf(-6.0 / 20.0);
        assert!((gain - 0.501).abs() < 1e-3);
        let before = decode(&orig, &d.format);
        let after = decode(&data, &d.format);
        for (b, a) in before.iter().zip(&after) {
            for i in 100..200 {
                assert!((a[i] - gain * b[i]).abs() < 1e-6, "frame {i}");
            }
        }
    }

    #[test]
    fn overlapping_effect_ranges_leave_everything_else_alone() {
        let bytes = noisy_32bit(&[sine(441.0, RATE, FRAMES, 0.8)]);
        let d = doc(&bytes);
        let e = with_effect(
            &d,
            d.initial_edit(),
            Effect::Gain { db: -6.0 },
            Some((100, 199)),
        );
        let e = with_effect(&d, e, Effect::Invert, Some((150, 249)));
        let orig = data_of(&bytes);
        let data = data_of(&build_wav_bytes(&d, &e));
        assert_eq!(data[..100 * 4], orig[..100 * 4]);
        assert_eq!(data[250 * 4..], orig[250 * 4..]);
        let want = process(&d.audio, &e.effects);
        let got = decode(&data, &d.format);
        for i in 100..250 {
            assert!((got[0][i] - want[0][i]).abs() < 1e-6, "frame {i}");
        }
    }

    #[test]
    fn effects_then_crossfade_then_trim() {
        let d = doc(&file(vec![], vec![]));
        let e = EditState {
            crossfade: 50,
            start: 200,
            ..looped(&d, 1000, 1999)
        };
        let e = with_effect(&d, e, Effect::Invert, None).clamped(d.frames);
        assert_eq!(e.crossfade, 50);

        let rendered = render(&process(&d.audio, &e.effects), &e);
        assert_ne!(rendered, d.audio, "the test must change the audio");
        let want = &rendered[0][e.start..=e.end];
        let got = decode(&data_of(&build_wav_bytes(&d, &e)), &d.format);
        assert_eq!(got[0].len(), want.len());
        for (i, (g, w)) in got[0].iter().zip(want).enumerate() {
            // The equal-power crossfade of two near-identical waves can clip.
            let w = w.clamp(-1.0, 32767.0 / 32768.0);
            assert!((g - w).abs() <= LSB, "frame {i}: {g} vs {w}");
        }
    }

    #[test]
    fn stereo_to_mono_rewrites_fmt() {
        let d = doc(&stereo_file());
        let e = with_effect(&d, d.initial_edit(), Effect::StereoToMono, None);
        let out = build_wav_bytes(&d, &e);

        let f = parse_fmt(&fmt_of(&out)).unwrap();
        assert_eq!((f.channels, f.block_align), (1, 2));
        assert_eq!(u32_at(&fmt_of(&out), 8), RATE * 2);
        let back = doc(&out);
        assert_eq!(back.frames, FRAMES);
        assert_eq!(back.audio.len(), 1);
        for i in 0..FRAMES {
            let avg = (d.audio[0][i] + d.audio[1][i]) / 2.0;
            assert!((back.audio[0][i] - avg).abs() <= LSB, "frame {i}");
        }
    }

    #[test]
    fn mono_to_stereo_rewrites_fmt() {
        let d = doc(&file(vec![], vec![]));
        let e = with_effect(&d, d.initial_edit(), Effect::MonoToStereo, None);
        let out = build_wav_bytes(&d, &e);

        let f = parse_fmt(&fmt_of(&out)).unwrap();
        assert_eq!((f.channels, f.block_align), (2, 4));
        assert_eq!(u32_at(&fmt_of(&out), 8), RATE * 4);
        let back = doc(&out);
        assert_eq!(back.frames, FRAMES);
        assert_eq!(back.audio.len(), 2);
        assert_eq!(back.audio[0], d.audio[0]);
        assert_eq!(back.audio[1], d.audio[0]);
    }

    #[test]
    fn channel_change_with_trim_and_crossfade_stays_aligned() {
        let d = doc(&stereo_file());
        let e = EditState {
            start: 100,
            end: 3000,
            crossfade: 20,
            ..looped(&d, 1000, 2000)
        };
        let e = with_effect(&d, e, Effect::StereoToMono, None);
        let rendered = render(&process(&d.audio, &e.effects), &e);
        let back = doc(&build_wav_bytes(&d, &e));
        assert_eq!(back.audio.len(), 1);
        assert_eq!(back.frames, 2901);
        for (i, v) in back.audio[0].iter().enumerate() {
            assert!((v - rendered[0][100 + i]).abs() <= LSB, "frame {i}");
        }
    }

    #[test]
    fn extensible_channel_change_rewrites_the_mask() {
        let mut riff = parse_riff(&wav_bytes(
            &[
                sine(441.0, RATE, FRAMES, 0.8),
                sine(330.0, RATE, FRAMES, 0.4),
            ],
            RATE,
            SampleFormat::Int { bits: 24 },
            vec![],
        ))
        .unwrap();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&RATE.to_le_bytes());
        fmt.extend_from_slice(&(RATE * 6).to_le_bytes());
        fmt.extend_from_slice(&6u16.to_le_bytes());
        fmt.extend_from_slice(&24u16.to_le_bytes());
        fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        fmt.extend_from_slice(&24u16.to_le_bytes()); // valid bits
        fmt.extend_from_slice(&3u32.to_le_bytes()); // channel mask
        fmt.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
            0x9B, 0x71,
        ]);
        assert_eq!(fmt.len(), 40);
        let i = find(&riff.chunks, b"fmt ").unwrap();
        riff.chunks[i].data = fmt.clone();
        let d = doc(&write_riff(&riff));
        assert_eq!(d.format.channels, 2);

        let e = with_effect(&d, d.initial_edit(), Effect::StereoToMono, None);
        let out = build_wav_bytes(&d, &e);
        let saved = fmt_of(&out);
        assert_eq!(saved.len(), 40);
        let f = parse_fmt(&saved).unwrap();
        assert_eq!((f.channels, f.block_align), (1, 3));
        assert_eq!(u32_at(&saved, 8), RATE * 3);
        assert_eq!(u32_at(&saved, 20), 0x4);
        assert_eq!(saved[24..], fmt[24..]);
        let back = doc(&out);
        assert_eq!((back.audio.len(), back.frames), (1, FRAMES));

        // And back to stereo: the mask returns to front left and right.
        let e2 = with_effect(&back, back.initial_edit(), Effect::MonoToStereo, None);
        assert_eq!(u32_at(&fmt_of(&build_wav_bytes(&back, &e2)), 20), 0x3);
    }

    #[test]
    fn reverse_whole_mirrors_extra_smpl_loops() {
        let audio = vec![sine(441.0, RATE, 1000, 0.8)];
        let bytes = wav_bytes(
            &audio,
            RATE,
            I16,
            vec![smpl_chunk(vec![fwd(100, 199), fwd(200, 300)])],
        );
        let d = doc(&bytes);
        assert_eq!(d.frames, 1000);
        let e = d.initial_edit();
        let a = plan_effect(Effect::Reverse, &d.audio, &e, None).unwrap();
        let mut e = mirror_edit(&e, a.first, a.last);
        e.effects.push(a);

        let out = build_wav_bytes(&d, &e);
        let s = smpl_of(&out).unwrap();
        assert_eq!(s.loops, vec![fwd(800, 899), fwd(699, 799)]);
        let want: Vec<f32> = d.audio[0].iter().rev().copied().collect();
        assert_eq!(doc(&out).audio[0], want);
    }

    #[test]
    fn unchanged_smpl_is_kept_byte_for_byte() {
        let original = Smpl {
            manufacturer: 0x47,
            product: 7,
            sample_period: 12345,
            unity_note: 60,
            pitch_fraction: 0,
            smpte_format: 0,
            smpte_offset: 0,
            loops: vec![
                SmplLoop {
                    cue_id: 7,
                    ..fwd(2532, 2915)
                },
                fwd(3000, 3100),
            ],
            sampler_data: vec![1, 2, 3, 4],
        }
        .to_bytes();
        let bytes = file(vec![chunk(b"smpl", original.clone())], vec![]);
        let d = doc(&bytes);
        let smpl_bytes = |e: &EditState| {
            let c = chunks(&build_wav_bytes(&d, e));
            c[find(&c, b"smpl").unwrap()].data.clone()
        };
        let init = d.initial_edit();
        assert_eq!(smpl_bytes(&init), original);

        // Effects, a tail trim and a crossfade do not touch the chunk.
        let with_fx = with_effect(&d, init.clone(), Effect::Invert, None);
        assert_eq!(smpl_bytes(&with_fx), original);
        let tail = EditState {
            end: 3500,
            crossfade: 20,
            ..init.clone()
        };
        assert_eq!(smpl_bytes(&tail), original);

        // A moved loop end rebuilds it: the sample period is recomputed, the cue id reset.
        let moved = EditState {
            loop_end: 2900,
            ..init.clone()
        };
        let rebuilt = smpl_bytes(&moved);
        assert_ne!(rebuilt, original);
        let s = Smpl::parse(&rebuilt).unwrap();
        assert_eq!(s.sample_period, sample_period(RATE));
        assert_eq!(s.loops[0], fwd(2532, 2900));

        // So do a new root, a head trim, a mirroring reverse and an extra loop cut off.
        let rooted = EditState {
            root: 61,
            ..init.clone()
        };
        assert_ne!(smpl_bytes(&rooted), original);
        let head = EditState {
            start: 10,
            ..init.clone()
        };
        assert_ne!(smpl_bytes(&head), original);
        let mut mirrored = init.clone();
        mirrored.effects.push(AppliedEffect {
            op: Op::Reverse {
                mirror_markers: true,
            },
            first: 0,
            last: 10,
        });
        assert_ne!(smpl_bytes(&mirrored), original);
        let cut = EditState { end: 2915, ..init };
        let s = Smpl::parse(&smpl_bytes(&cut)).unwrap();
        assert_eq!(s.loops.len(), 1);
        assert_eq!(s.loops[0].cue_id, 0);
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

        // Hand-made effects with ranges far outside the file, inverted or in the wrong channel
        // count are clamped or ignored.
        let mut e = d.initial_edit();
        for (first, last) in [(usize::MAX, usize::MAX), (500, 100), (3999, usize::MAX)] {
            e.effects.push(AppliedEffect {
                op: Op::Invert,
                first,
                last,
            });
        }
        for op in [Op::SwapChannels, Op::MonoToStereo, Op::StereoToMono] {
            e.effects.push(AppliedEffect {
                op,
                first: 7,
                last: 3,
            });
        }
        let out = doc(&build_wav_bytes(&d, &e));
        assert_eq!((out.frames, out.audio.len()), (FRAMES, 1));

        // A marker-mirroring reverse running far past the end, with an extra loop to map.
        let d = doc(&file(
            vec![smpl_chunk(vec![fwd(2532, 2915), fwd(100, 200)])],
            vec![],
        ));
        let mut e = d.initial_edit();
        e.effects.push(AppliedEffect {
            op: Op::Reverse {
                mirror_markers: true,
            },
            first: 1,
            last: usize::MAX,
        });
        let out = doc(&build_wav_bytes(&d, &e));
        assert_eq!(out.frames, FRAMES);
        // The same as the reverse clamped to the file: 1..=3999.
        let s = out.smpl.unwrap();
        assert_eq!(s.loops[1], fwd(3800, 3900));
        let mut want = d.audio[0].clone();
        want[1..].reverse();
        let got = &out.audio[0];
        assert!(got.iter().zip(&want).all(|(g, w)| (g - w).abs() <= LSB));

        let empty = wav_bytes(&[Vec::new()], RATE, I16, vec![]);
        let d = doc(&empty);
        assert_eq!(d.frames, 0);
        for e in [d.initial_edit(), looped(&d, 5, 50)] {
            let out = build_wav_bytes(&d, &e);
            assert!(data_of(&out).is_empty());
        }
    }
}
