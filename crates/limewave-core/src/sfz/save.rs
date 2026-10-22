//! Saving an instrument: the `.sfz` and the WAVs it plays (spec 7).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use super::bind::{Binding, write_back};
use super::model::Instrument;
use super::write::write;
use crate::document::WavDocument;
use crate::edit::EditState;
use crate::error::SaveError;
use crate::save::{
    Destination, SaveOutcome, backup_existing, build_wav_bytes, make_backup, same_file,
    write_atomic,
};

/// One loaded sample being saved.
pub struct SampleSave<'a> {
    pub doc: &'a WavDocument,
    /// The state the instrument's opcodes currently describe: the first entry of the sample's
    /// history.
    pub initial: &'a EditState,
    /// The sample as edited now.
    pub edit: &'a EditState,
}

/// What a successful instrument save wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentSaved {
    pub sfz: PathBuf,
    /// The `.sfz` first, then each WAV written or copied.
    pub written: Vec<SaveOutcome>,
}

/// Whether the WAV must be re-encoded to hold `e`: it differs from what the file said on load.
pub fn needs_rewrite(doc: &WavDocument, e: &EditState) -> bool {
    *e != doc.initial_edit()
}

/// The edit to write into the sample's WAV. With `write_smpl` off the WAV keeps the root key,
/// fine tune and loop it came with (those now live in the instrument); everything else (audio,
/// trim, crossfade) is the sample's current edit.
pub fn wav_edit_for_save(doc: &WavDocument, edit: &EditState, write_smpl: bool) -> EditState {
    if write_smpl {
        return edit.clone();
    }
    let own = doc.initial_edit();
    EditState {
        root: own.root,
        cents: own.cents,
        loop_mode: own.loop_mode,
        loop_start: own.loop_start,
        loop_end: own.loop_end,
        ..edit.clone()
    }
}

/// `path` with `.` dropped and `..` folded into the part before it, without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `file` relative to `dir` when it lies inside it (after normalising both).
fn inside(dir: &Path, file: &Path) -> Option<PathBuf> {
    let rel = normalize(file)
        .strip_prefix(normalize(dir))
        .ok()?
        .to_path_buf();
    (!rel.components().any(|c| matches!(c, Component::ParentDir))).then_some(rel)
}

/// Where a sample is written, and the `sample=` text its regions need if that changed.
struct Plan {
    /// Index into the binding's samples.
    index: usize,
    source: PathBuf,
    target: PathBuf,
    moved_text: Option<String>,
}

/// The `sample=` text for `samples/<name>` as resolved against `default_path`.
fn moved_text(default_path: &str, name: &str) -> String {
    let d = default_path.replace('\\', "/");
    let parts: Vec<&str> = d
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    // A default path that is absolute or climbs out cannot be undone by a relative prefix.
    let plain = !d.starts_with('/') && !d.contains(':') && !parts.contains(&"..");
    let up = if plain {
        "../".repeat(parts.len())
    } else {
        String::new()
    };
    format!("{up}samples/{name}")
}

/// The first free `name`, `stem (2).ext`, `stem (3).ext`, … in `samples/`; the chosen name is
/// recorded in `taken` (compared ignoring case).
fn free_name(taken: &mut HashSet<String>, name: &std::ffi::OsStr) -> String {
    let path = Path::new(name);
    let stem = path.file_stem().unwrap_or(name).to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| match n {
            1 => format!("{stem}{ext}"),
            n => format!("{stem} ({n}){ext}"),
        })
        .find(|candidate| taken.insert(format!("samples/{}", candidate.to_lowercase())))
        .unwrap_or_default()
}

/// Targets for a save into `target_dir`: samples inside the instrument's folder keep their
/// place under it; the rest are gathered in `samples/` (spec 7.1).
fn plan_into(
    binding: &Binding,
    sfz_dir: &Path,
    target_dir: &Path,
    default_path: &str,
) -> Vec<Plan> {
    let mut taken = HashSet::new();
    let mut plans: Vec<Option<Plan>> = binding.samples.iter().map(|_| None).collect();
    // Samples keeping their place go first, so they hold their names against moved ones.
    for (index, s) in binding.samples.iter().enumerate() {
        if let Some(rel) = inside(sfz_dir, &s.path) {
            taken.insert(rel.to_string_lossy().replace('\\', "/").to_lowercase());
            plans[index] = Some(Plan {
                index,
                source: s.path.clone(),
                target: target_dir.join(rel),
                moved_text: None,
            });
        }
    }
    for (index, s) in binding.samples.iter().enumerate() {
        if plans[index].is_none() {
            let name = free_name(&mut taken, s.path.file_name().unwrap_or_default());
            plans[index] = Some(Plan {
                index,
                source: s.path.clone(),
                target: target_dir.join("samples").join(&name),
                moved_text: Some(moved_text(default_path, &name)),
            });
        }
    }
    plans.into_iter().flatten().collect()
}

/// Creates the folder `target` goes into.
fn create_parent(target: &Path) -> Result<(), SaveError> {
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Saves the instrument and its samples (spec 7).
///
/// `sfz_path` is where the instrument was read from (or the picked path when untitled), and
/// `samples` is parallel to `binding.samples`: `None` for a sample that is not loaded, which is
/// then copied or left as it is. Sample edits are written into the instrument's opcodes first.
/// `Folder` and `File` write the `.sfz` and every sample under the new place; the two overwrite
/// kinds write over the originals and skip unchanged WAVs. The WAVs are written first and the
/// `.sfz` last.
pub fn save_instrument(
    inst: &Instrument,
    sfz_path: &Path,
    binding: &Binding,
    samples: &[Option<SampleSave>],
    dest: &Destination,
    write_smpl: bool,
) -> Result<InstrumentSaved, SaveError> {
    let mut out = inst.clone();
    for (sample, save) in binding.samples.iter().zip(samples) {
        if let Some(s) = save {
            write_back(&mut out, sample, s.initial, s.edit);
        }
    }

    let sfz_target = match dest {
        Destination::Folder(dir) => {
            let name = sfz_path
                .file_name()
                .ok_or_else(|| SaveError::NoFileName(sfz_path.to_path_buf()))?;
            dir.join(name)
        }
        Destination::OverwriteWithBackup | Destination::ReplaceSaved => sfz_path.to_path_buf(),
        Destination::File(p) => p.clone(),
    };
    let plans = match dest {
        Destination::OverwriteWithBackup | Destination::ReplaceSaved => binding
            .samples
            .iter()
            .enumerate()
            .map(|(index, s)| Plan {
                index,
                source: s.path.clone(),
                target: s.path.clone(),
                moved_text: None,
            })
            .collect(),
        Destination::Folder(_) | Destination::File(_) => plan_into(
            binding,
            sfz_path.parent().unwrap_or(Path::new("")),
            sfz_target.parent().unwrap_or(Path::new("")),
            &inst.default_path(),
        ),
    };
    if matches!(dest, Destination::Folder(_)) {
        let clash = std::iter::once((&sfz_target, sfz_path))
            .chain(plans.iter().map(|p| (&p.target, p.source.as_path())))
            .find(|(target, source)| same_file(target, source));
        if let Some((target, _)) = clash {
            return Err(SaveError::WouldOverwriteOriginal(target.clone()));
        }
    }
    for plan in &plans {
        if let Some(text) = &plan.moved_text {
            for &region in &binding.samples[plan.index].regions {
                out.set(region, "sample", text);
            }
        }
    }

    let mut written = Vec::new();
    for plan in &plans {
        let save = samples.get(plan.index).and_then(Option::as_ref);
        let rewritten = save.and_then(|s| {
            let e = wav_edit_for_save(s.doc, s.edit, write_smpl);
            needs_rewrite(s.doc, &e).then(|| build_wav_bytes(s.doc, &e))
        });
        let in_place = same_file(&plan.target, &plan.source);
        let bytes = match rewritten {
            Some(b) => b,
            None if in_place => continue,
            None => std::fs::read(&plan.source)?,
        };
        let backup = if in_place {
            match dest {
                Destination::ReplaceSaved => None,
                _ => Some(make_backup(&plan.target)?),
            }
        } else {
            create_parent(&plan.target)?;
            backup_existing(&plan.target)?
        };
        write_atomic(&plan.target, &plan.source, &bytes)?;
        written.push(SaveOutcome {
            written: plan.target.clone(),
            backup,
        });
    }

    create_parent(&sfz_target)?;
    let backup = match dest {
        Destination::ReplaceSaved => None,
        _ => backup_existing(&sfz_target)?,
    };
    let source = if sfz_path.exists() {
        sfz_path
    } else {
        &sfz_target
    };
    write_atomic(&sfz_target, source, write(&out.file).as_bytes())?;
    written.insert(
        0,
        SaveOutcome {
            written: sfz_target.clone(),
            backup,
        },
    );
    Ok(InstrumentSaved {
        sfz: sfz_target,
        written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::load_wav;
    use crate::edit::LoopMode;
    use crate::riff::{Chunk, parse_riff};
    use crate::sfz::bind::{bind, region_edit};
    use crate::sfz::model::Instrument;
    use crate::sfz::parse::{decode_text, parse};
    use crate::smpl::{Smpl, sample_period};
    use crate::testwav::{sine, wav_bytes};
    use crate::wav::SampleFormat;

    const RATE: u32 = 44100;
    const FRAMES: usize = 4000;
    const I16: SampleFormat = SampleFormat::Int { bits: 16 };

    fn smpl_chunk(root: u32) -> Chunk {
        let s = Smpl {
            manufacturer: 0,
            product: 0,
            sample_period: sample_period(RATE),
            unity_note: root,
            pitch_fraction: 0,
            smpte_format: 0,
            smpte_offset: 0,
            loops: Vec::new(),
            sampler_data: Vec::new(),
        };
        Chunk {
            id: *b"smpl",
            data: s.to_bytes(),
        }
    }

    fn wav_with(freq: f32, extra: Vec<Chunk>) -> Vec<u8> {
        wav_bytes(&[sine(freq, RATE, FRAMES, 0.8)], RATE, I16, extra)
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// An instrument read from a file, with its samples loaded.
    struct Open {
        sfz: PathBuf,
        inst: Instrument,
        binding: Binding,
        docs: Vec<WavDocument>,
        initial: Vec<EditState>,
        edits: Vec<EditState>,
    }

    fn open(sfz: &Path) -> Open {
        let inst = Instrument::new(parse(&decode_text(&std::fs::read(sfz).unwrap())).unwrap());
        let binding = bind(&inst, sfz.parent().unwrap());
        let docs: Vec<WavDocument> = binding
            .samples
            .iter()
            .map(|s| load_wav(&s.path).unwrap())
            .collect();
        let initial: Vec<EditState> = binding
            .samples
            .iter()
            .zip(&docs)
            .map(|(s, d)| region_edit(&inst, s.regions[0], &d.initial_edit()))
            .collect();
        Open {
            sfz: sfz.to_path_buf(),
            inst,
            binding,
            docs,
            edits: initial.clone(),
            initial,
        }
    }

    impl Open {
        fn save(&self, dest: &Destination, write_smpl: bool) -> Result<InstrumentSaved, SaveError> {
            let samples: Vec<Option<SampleSave>> = (0..self.docs.len())
                .map(|i| {
                    Some(SampleSave {
                        doc: &self.docs[i],
                        initial: &self.initial[i],
                        edit: &self.edits[i],
                    })
                })
                .collect();
            save_instrument(
                &self.inst,
                &self.sfz,
                &self.binding,
                &samples,
                dest,
                write_smpl,
            )
        }
    }

    fn text(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    fn root_of(p: &Path) -> Option<u32> {
        let chunks = parse_riff(&std::fs::read(p).unwrap()).unwrap().chunks;
        chunks
            .iter()
            .find(|c| &c.id == b"smpl")
            .map(|c| Smpl::parse(&c.data).unwrap().unity_note)
    }

    fn looped(e: &EditState) -> EditState {
        EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 200,
            loop_end: 800,
            ..e.clone()
        }
    }

    fn reopen_all_found(sfz: &Path) -> Binding {
        let o = open(sfz);
        assert!(
            o.binding.region_sample.iter().all(Option::is_some),
            "{:?}",
            o.binding.region_sample
        );
        o.binding
    }

    #[test]
    fn new_folder_save_is_complete_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let x = wav_with(220.0, vec![]);
        put(root, "inst/Samples/x.wav", &x);
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<region> sample=Samples\\x.wav key=60\n<region> sample=samples/X.wav key=62\n",
        );
        let mut o = open(&sfz);
        o.edits[0] = looped(&o.initial[0]);
        let out = root.join("inst/limewave");
        let saved = o.save(&Destination::Folder(out.clone()), false).unwrap();

        assert_eq!(saved.sfz, out.join("a.sfz"));
        assert_eq!(saved.written[0].written, out.join("a.sfz"));
        assert_eq!(saved.written.len(), 2);
        assert_eq!(std::fs::read(out.join("Samples/x.wav")).unwrap(), x);
        let b = reopen_all_found(&out.join("a.sfz"));
        assert_eq!(b.samples.len(), 1);
        assert!(text(&out.join("a.sfz")).contains("loop_start=200"));
        // The original instrument is untouched.
        assert!(!text(&sfz).contains("loop_start"));
    }

    #[test]
    fn default_path_instrument_reopens_with_moved_sample() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "inst/Kit/a.wav", &wav_with(220.0, vec![]));
        put(root, "shared/y.wav", &wav_with(330.0, vec![]));
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<control> default_path=Kit/\n<region> sample=a.wav key=60\n<region> sample=../../shared/y.wav key=62\n",
        );
        let o = open(&sfz);
        assert!(o.binding.region_sample.iter().all(Option::is_some));
        let out = root.join("inst/limewave");
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        reopen_all_found(&out.join("a.sfz"));
        assert!(out.join("samples/y.wav").is_file());
    }

    #[test]
    fn outside_samples_move_into_samples_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let y = wav_with(330.0, vec![]);
        put(root, "shared/y.wav", &y);
        put(root, "inst/z.wav", &wav_with(110.0, vec![]));
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<region> sample=../shared/y.wav key=60\n<region> sample=z.wav key=62\n",
        );
        let o = open(&sfz);
        let out = root.join("inst/limewave");
        o.save(&Destination::Folder(out.clone()), false).unwrap();

        assert_eq!(std::fs::read(out.join("samples/y.wav")).unwrap(), y);
        assert!(out.join("z.wav").is_file());
        let saved = text(&out.join("a.sfz"));
        assert!(saved.contains("sample=samples/y.wav"), "{saved}");
        assert!(saved.contains("sample=z.wav"), "{saved}");
        reopen_all_found(&out.join("a.sfz"));
    }

    #[test]
    fn outside_samples_with_one_name_get_numbered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "s1/y.wav", &wav_with(100.0, vec![]));
        put(root, "s2/y.wav", &wav_with(200.0, vec![]));
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<region> sample=../s1/y.wav key=60\n<region> sample=../s2/y.wav key=62\n",
        );
        let o = open(&sfz);
        let out = root.join("inst/limewave");
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        assert!(out.join("samples/y.wav").is_file());
        assert!(out.join("samples/y (2).wav").is_file());
        let b = reopen_all_found(&out.join("a.sfz"));
        assert_eq!(b.samples.len(), 2);
    }

    #[test]
    fn unchanged_wavs_are_copied_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // An odd chunk a re-encode would drop.
        let odd = Chunk {
            id: *b"zzzz",
            data: vec![1, 2, 3, 4],
        };
        let x = wav_with(220.0, vec![odd]);
        put(root, "inst/x.wav", &x);
        let sfz = put(root, "inst/a.sfz", b"<region> sample=x.wav\n");
        let o = open(&sfz);
        let out = root.join("inst/limewave");
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        assert_eq!(std::fs::read(out.join("x.wav")).unwrap(), x);
    }

    #[test]
    fn overwrite_backs_up_sfz_and_changed_wavs_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = wav_with(220.0, vec![]);
        let b = wav_with(330.0, vec![]);
        let pa = put(root, "inst/a.wav", &a);
        let pb = put(root, "inst/b.wav", &b);
        let sfz = put(
            root,
            "inst/i.sfz",
            b"<region> sample=a.wav key=60\n<region> sample=b.wav key=62\n",
        );
        let mut o = open(&sfz);
        o.edits[0].end = 2999;
        let saved = o.save(&Destination::OverwriteWithBackup, false).unwrap();

        assert_eq!(saved.written.len(), 2);
        assert_eq!(saved.written[0].written, sfz);
        assert!(saved.written[0].backup.is_some());
        assert_eq!(saved.written[1].written, pa);
        assert!(saved.written[1].backup.is_some());
        assert_eq!(std::fs::read(root.join("inst/a.wav.bak")).unwrap(), a);
        assert_ne!(std::fs::read(&pa).unwrap(), a);
        assert!(root.join("inst/i.sfz.bak").is_file());
        assert!(!root.join("inst/b.wav.bak").exists());
        assert_eq!(std::fs::read(&pb).unwrap(), b);
    }

    #[test]
    fn replace_saved_writes_without_backups() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "inst/a.wav", &wav_with(220.0, vec![]));
        put(root, "inst/b.wav", &wav_with(330.0, vec![]));
        let sfz = put(
            root,
            "inst/i.sfz",
            b"<region> sample=a.wav key=60\n<region> sample=b.wav key=62\n",
        );
        let mut o = open(&sfz);
        o.edits[0].end = 2999;
        let saved = o.save(&Destination::ReplaceSaved, false).unwrap();
        // The sfz and the changed WAV; the unchanged one is skipped.
        assert_eq!(saved.written.len(), 2);
        assert!(saved.written.iter().all(|w| w.backup.is_none()));
        assert!(!root.join("inst/a.wav.bak").exists());
        assert!(!root.join("inst/i.sfz.bak").exists());
    }

    #[test]
    fn file_destination_puts_samples_beside_the_picked_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "inst/Samples/x.wav", &wav_with(220.0, vec![]));
        let sfz = put(root, "inst/a.sfz", b"<region> sample=Samples/x.wav\n");
        let o = open(&sfz);
        let picked = root.join("elsewhere/b.sfz");
        let saved = o.save(&Destination::File(picked.clone()), false).unwrap();
        assert_eq!(saved.sfz, picked);
        assert!(root.join("elsewhere/Samples/x.wav").is_file());
        reopen_all_found(&picked);
    }

    #[test]
    fn folder_refuses_to_overwrite_the_originals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let x = wav_with(220.0, vec![]);
        put(root, "inst/x.wav", &x);
        let sfz = put(root, "inst/a.sfz", b"<region> sample=x.wav\n");
        let before = std::fs::read(&sfz).unwrap();
        let o = open(&sfz);
        let err = o
            .save(&Destination::Folder(root.join("inst")), false)
            .unwrap_err();
        assert!(matches!(err, SaveError::WouldOverwriteOriginal(_)));
        assert_eq!(std::fs::read(&sfz).unwrap(), before);
        assert_eq!(std::fs::read(root.join("inst/x.wav")).unwrap(), x);
    }

    #[test]
    fn smpl_off_keeps_the_wavs_own_tuning() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "inst/x.wav", &wav_with(220.0, vec![smpl_chunk(60)]));
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<region> sample=x.wav pitch_keycenter=49\n",
        );
        for (trim, out) in [(false, "o1"), (true, "o2")] {
            let mut o = open(&sfz);
            assert_eq!(o.initial[0].root, 49);
            o.edits[0] = looped(&o.initial[0]);
            if trim {
                o.edits[0].end = 2999;
            }
            let out = root.join(out);
            o.save(&Destination::Folder(out.clone()), false).unwrap();
            assert_eq!(root_of(&out.join("x.wav")), Some(60), "trim {trim}");
            let saved = text(&out.join("a.sfz"));
            assert!(saved.contains("loop_start=200"), "{saved}");
            assert!(saved.contains("loop_end=800"), "{saved}");
            assert!(saved.contains("pitch_keycenter=49"), "{saved}");
        }
    }

    #[test]
    fn smpl_on_writes_instrument_tuning_into_wav() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "inst/x.wav", &wav_with(220.0, vec![smpl_chunk(60)]));
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<region> sample=x.wav pitch_keycenter=49\n",
        );
        let o = open(&sfz);
        let out = root.join("out");
        o.save(&Destination::Folder(out.clone()), true).unwrap();
        assert_eq!(root_of(&out.join("x.wav")), Some(49));
    }

    #[test]
    fn missing_sample_is_kept_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "inst/x.wav", &wav_with(220.0, vec![]));
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<region> sample=x.wav key=60\n<region> sample=Gone\\g.wav key=62\n",
        );
        let o = open(&sfz);
        assert_eq!(o.binding.region_sample, [Some(0), None]);
        let out = root.join("inst/limewave");
        let saved = o.save(&Destination::Folder(out.clone()), false).unwrap();
        assert_eq!(saved.written.len(), 2);
        assert!(out.join("x.wav").is_file());
        assert!(!out.join("Gone").exists());
        assert!(!out.join("samples").exists());
        assert!(text(&out.join("a.sfz")).contains("sample=Gone\\g.wav"));
    }
}
