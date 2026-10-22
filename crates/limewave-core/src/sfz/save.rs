//! Saving an instrument: the `.sfz` and the WAVs it plays (spec 7).

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use super::bind::{Binding, write_back};
use super::model::Instrument;
use super::paths::{is_absolute_text, normalize, same_file_key, sample_path, stays_inside};
use super::write::write;
use crate::document::WavDocument;
use crate::edit::EditState;
use crate::error::SaveError;
use crate::save::{
    Destination, SaveOutcome, backup_existing, build_wav_bytes_with_smpl, make_backup, smpl_state,
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

/// Whether the WAV must be re-encoded to hold the audio of `e` with the root key, fine tune and
/// loop of `smpl` in its chunks: they differ from what the file said on load.
pub fn needs_rewrite(doc: &WavDocument, e: &EditState, smpl: &EditState) -> bool {
    smpl_state(e, smpl) != doc.initial_edit()
}

/// The root key, fine tune and loop the sample's WAV is to say in `smpl`: the sample's edit
/// with `write_smpl` on, else those the WAV came with (they now live in the instrument). The
/// audio, trim and crossfade are always the edit's (spec 7.3).
pub fn smpl_edit_for_save<'a>(
    doc: &WavDocument,
    edit: &'a EditState,
    write_smpl: bool,
) -> std::borrow::Cow<'a, EditState> {
    if write_smpl {
        std::borrow::Cow::Borrowed(edit)
    } else {
        std::borrow::Cow::Owned(doc.initial_edit())
    }
}

/// `file` relative to `dir` when it lies inside it (after normalising both).
fn inside(dir: &Path, file: &Path) -> Option<PathBuf> {
    let rel = normalize(file)
        .strip_prefix(normalize(dir))
        .ok()?
        .to_path_buf();
    (!rel.components().any(|c| matches!(c, Component::ParentDir))).then_some(rel)
}

/// `target` as a `/` path from the folder `base`, climbing with `..`; `None` when there is no
/// such path (different drives or roots, or `base` itself climbs).
fn relative_text(base: &Path, target: &Path) -> Option<String> {
    let (base, target) = (normalize(base), normalize(target));
    let b: Vec<Component> = base.components().collect();
    let t: Vec<Component> = target.components().collect();
    let common = b.iter().zip(&t).take_while(|(x, y)| x == y).count();
    let rooted =
        |c: Option<&Component>| matches!(c, Some(Component::Prefix(_) | Component::RootDir));
    if common == 0 && (rooted(b.first()) || rooted(t.first())) {
        return None;
    }
    if !b[common..]
        .iter()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let mut parts: Vec<String> = vec!["..".to_owned(); b.len() - common];
    parts.extend(
        t[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    Some(parts.join("/"))
}

/// The `sample=` text that reaches `target` from a saved instrument in `target_dir` with this
/// `default_path`: a path relative to the default folder, or the absolute path when none exists.
pub fn moved_text(target_dir: &Path, default_path: &str, target: &Path) -> String {
    let dp = default_path.replace('\\', "/");
    let base = if is_absolute_text(&dp) {
        PathBuf::from(&dp)
    } else {
        target_dir.join(&dp)
    };
    relative_text(&base, target).unwrap_or_else(|| target.to_string_lossy().replace('\\', "/"))
}

/// Where a sample is written, and the `sample=` text its regions need if that changed.
struct Plan {
    /// Index into the binding's samples.
    index: usize,
    source: PathBuf,
    target: PathBuf,
    moved_text: Option<String>,
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

/// Targets for a save into `target_dir`. A sample keeps its place under it only when every
/// region names it by a relative text that stays inside the instrument's folder; any other
/// (a `..` path, an absolute path or default path) is gathered in `samples/` (spec 7.1).
fn plan_into(inst: &Instrument, binding: &Binding, sfz_dir: &Path, target_dir: &Path) -> Vec<Plan> {
    let default_path = inst.default_path();
    let mut taken = HashSet::new();
    let mut plans: Vec<Option<Plan>> = binding.samples.iter().map(|_| None).collect();
    // Samples keeping their place go first, so they hold their names against moved ones.
    for (index, s) in binding.samples.iter().enumerate() {
        let relative = s.regions.iter().all(|&r| {
            inst.region_info(r)
                .sample
                .is_some_and(|text| stays_inside(&default_path, &text))
        });
        let rel = if relative {
            inside(sfz_dir, &s.path)
        } else {
            None
        };
        if let Some(rel) = rel {
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
            let target = target_dir.join("samples").join(&name);
            plans[index] = Some(Plan {
                index,
                source: s.path.clone(),
                moved_text: Some(moved_text(target_dir, &default_path, &target)),
                target,
            });
        }
    }
    plans.into_iter().flatten().collect()
}

/// Where a save of the instrument into the folder `dir` writes: the `.sfz`, then each sample of
/// `binding` (spec 7.1). Lets the caller ask before files already there are replaced.
pub fn folder_targets(
    inst: &Instrument,
    sfz_path: &Path,
    binding: &Binding,
    dir: &Path,
) -> Vec<PathBuf> {
    let sfz_dir = sfz_path.parent().unwrap_or(Path::new(""));
    sfz_path
        .file_name()
        .map(|name| dir.join(name))
        .into_iter()
        .chain(
            plan_into(inst, binding, sfz_dir, dir)
                .into_iter()
                .map(|p| p.target),
        )
        .collect()
}

/// Creates the folder `target` goes into.
fn create_parent(target: &Path) -> Result<(), SaveError> {
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// What identifies a file: its resolved path, or the path as given when it cannot be resolved.
fn file_id(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// One WAV to write, decided before anything is touched.
struct Job {
    /// Index into the binding's samples.
    index: usize,
    source: PathBuf,
    target: PathBuf,
    /// Whether the WAV is copied as it is (else it is rebuilt).
    copy: bool,
    in_place: bool,
}

/// Saves the instrument and its samples (spec 7).
///
/// `sfz_path` is where the instrument was read from (or the picked path when untitled); it must
/// be absolute. `samples` is parallel to `binding.samples`: `None` for a sample that is not
/// loaded, which is then copied or left as it is. Sample edits are written into the
/// instrument's opcodes first.
///
/// `Folder` and `File` write the `.sfz` and every sample under the new place, so a saved copy
/// plays its own files. `Folder` refuses any target that is the same file as the source `.sfz`
/// or any sample. The two overwrite kinds write over the originals and skip unchanged WAVs.
/// `File` with a target that is its own source behaves the same way: unchanged files are
/// skipped, changed ones are overwritten after a `.bak`.
///
/// Nothing is replaced until everything that can fail beforehand has passed: the targets are
/// checked, every WAV to be copied is checked readable, and every backup (the `.sfz`'s too) is
/// made. Then the WAVs are written, and the `.sfz` last. A failed backup or read check aborts
/// with nothing replaced (backups already made stay).
pub fn save_instrument(
    inst: &Instrument,
    sfz_path: &Path,
    binding: &Binding,
    samples: &[Option<SampleSave>],
    dest: &Destination,
    write_smpl: bool,
) -> Result<InstrumentSaved, SaveError> {
    debug_assert!(sfz_path.is_absolute(), "{}", sfz_path.display());
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
    let moving = matches!(dest, Destination::Folder(_) | Destination::File(_));
    let plans = if moving {
        plan_into(
            inst,
            binding,
            sfz_path.parent().unwrap_or(Path::new("")),
            sfz_target.parent().unwrap_or(Path::new("")),
        )
    } else {
        binding
            .samples
            .iter()
            .enumerate()
            .map(|(index, s)| Plan {
                index,
                source: s.path.clone(),
                target: s.path.clone(),
                moved_text: None,
            })
            .collect()
    };

    if moving {
        // A target may be its own source only for `File` (see above); never another file's.
        let strict = matches!(dest, Destination::Folder(_));
        let mut by_id: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        let sources = std::iter::once(sfz_path).chain(plans.iter().map(|p| p.source.as_path()));
        for (i, source) in sources.enumerate() {
            by_id.entry(file_id(source)).or_default().push(i);
        }
        let targets = std::iter::once(&sfz_target).chain(plans.iter().map(|p| &p.target));
        for (i, target) in targets.enumerate() {
            let hits = by_id.get(&file_id(target));
            if hits.is_some_and(|h| h.iter().any(|&s| strict || s != i)) {
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
        // Every region must now reach its own target from the saved instrument.
        let target_dir = sfz_target.parent().unwrap_or(Path::new(""));
        let default_path = out.default_path();
        for plan in &plans {
            let want = same_file_key(&normalize(&plan.target));
            for &region in &binding.samples[plan.index].regions {
                let reached = out
                    .region_info(region)
                    .sample
                    .map(|text| same_file_key(&sample_path(target_dir, &default_path, &text)));
                if reached.as_deref() != Some(want.as_str()) {
                    return Err(SaveError::SamplePath(plan.source.clone()));
                }
            }
        }
    }

    // Moved samples are named relative to the default folder. When that lies inside the new
    // folder, it is created too, so a system that does not fold `..` in the text still
    // resolves it.
    let target_dir = sfz_target.parent().unwrap_or(Path::new(""));
    let default_path = out.default_path().replace('\\', "/");
    let default_folder = (plans.iter().any(|p| p.moved_text.is_some())
        && !is_absolute_text(&default_path))
    .then(|| normalize(&target_dir.join(&default_path)))
    .filter(|d| inside(target_dir, d).is_some_and(|rel| rel.components().next().is_some()));

    // Phase 1: decide each WAV, check what is copied can be read, make every backup.
    let mut jobs = Vec::new();
    for plan in plans {
        let rewrite = samples
            .get(plan.index)
            .and_then(Option::as_ref)
            .is_some_and(|s| {
                needs_rewrite(
                    s.doc,
                    s.edit,
                    &smpl_edit_for_save(s.doc, s.edit, write_smpl),
                )
            });
        let in_place = file_id(&plan.target) == file_id(&plan.source);
        if in_place && !rewrite {
            continue;
        }
        let copy = !rewrite;
        if copy {
            // Only a check that the source can be read; it is opened again to copy, so a big
            // instrument never holds many files open at once.
            drop(std::fs::File::open(&plan.source)?);
        }
        jobs.push(Job {
            index: plan.index,
            source: plan.source,
            target: plan.target,
            copy,
            in_place,
        });
    }
    let mut outcomes = Vec::new();
    for job in &jobs {
        create_parent(&job.target)?;
        let backup = if job.in_place {
            match dest {
                Destination::ReplaceSaved => None,
                _ => Some(make_backup(&job.target)?),
            }
        } else {
            backup_existing(&job.target)?
        };
        outcomes.push(SaveOutcome {
            written: job.target.clone(),
            backup,
        });
    }
    create_parent(&sfz_target)?;
    let sfz_backup = match dest {
        Destination::ReplaceSaved => None,
        _ => backup_existing(&sfz_target)?,
    };

    // Phase 2: write the WAVs, then the `.sfz`.
    if let Some(dir) = default_folder {
        std::fs::create_dir_all(dir)?;
    }
    for job in jobs {
        let bytes = if job.copy {
            std::fs::read(&job.source)?
        } else {
            let s = samples[job.index]
                .as_ref()
                .expect("a rebuilt sample is loaded");
            build_wav_bytes_with_smpl(
                s.doc,
                s.edit,
                &smpl_edit_for_save(s.doc, s.edit, write_smpl),
            )
        };
        write_atomic(&job.target, &job.source, &bytes)?;
    }
    let source = if sfz_path.exists() {
        sfz_path
    } else {
        &sfz_target
    };
    write_atomic(&sfz_target, source, write(&out.file).as_bytes())?;
    outcomes.insert(
        0,
        SaveOutcome {
            written: sfz_target.clone(),
            backup: sfz_backup,
        },
    );
    Ok(InstrumentSaved {
        sfz: sfz_target,
        written: outcomes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::load_wav;
    use crate::edit::LoopMode;
    use crate::riff::{Chunk, parse_riff};
    use crate::save::build_wav_bytes;
    use crate::sfz::bind::{bind, region_edit};
    use crate::sfz::model::Instrument;
    use crate::sfz::parse::{decode_text, parse};
    use crate::sfz::paths::normalize;
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

    /// Reopens a saved instrument: every region's sample is found, and each lies under the
    /// folder of the `.sfz` (a copy plays its own files, not the originals).
    fn reopen_all_found(sfz: &Path) -> Binding {
        let o = open(sfz);
        assert!(
            o.binding.region_sample.iter().all(Option::is_some),
            "{:?}",
            o.binding.region_sample
        );
        for s in &o.binding.samples {
            assert!(
                normalize(&s.path).starts_with(normalize(sfz.parent().unwrap())),
                "{} is outside {}",
                s.path.display(),
                sfz.display()
            );
        }
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
    fn folder_targets_are_where_a_folder_save_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "shared/y.wav", &wav_with(330.0, vec![]));
        put(root, "inst/Samples/z.wav", &wav_with(110.0, vec![]));
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<region> sample=../shared/y.wav key=60\n<region> sample=Samples/z.wav key=62\n",
        );
        let o = open(&sfz);
        let out = root.join("inst/limewave");
        let mut targets: Vec<PathBuf> = folder_targets(&o.inst, &sfz, &o.binding, &out)
            .iter()
            .map(|p| normalize(p))
            .collect();
        let saved = o.save(&Destination::Folder(out.clone()), false).unwrap();
        let mut written: Vec<PathBuf> = saved
            .written
            .iter()
            .map(|w| normalize(&w.written))
            .collect();
        targets.sort();
        written.sort();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets, written);
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
    fn absolute_sample_inside_the_folder_is_moved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let x = put(root, "inst/Samples/x.wav", &wav_with(220.0, vec![]));
        let abs = x.to_string_lossy().replace('\\', "/");
        let sfz = put(
            root,
            "inst/a.sfz",
            format!("<region> sample={abs} key=60\n").as_bytes(),
        );
        let o = open(&sfz);
        assert_eq!(o.binding.samples.len(), 1);
        let out = root.join("inst/limewave");
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        let saved = text(&out.join("a.sfz"));
        assert!(saved.contains("sample=samples/x.wav"), "{saved}");
        assert!(out.join("samples/x.wav").is_file());
        reopen_all_found(&out.join("a.sfz"));
    }

    #[test]
    fn default_path_climbing_out_moves_every_sample() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = wav_with(220.0, vec![]);
        put(root, "Samples/a.wav", &a);
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<control> default_path=../Samples/\n<region> sample=a.wav key=60\n",
        );
        let o = open(&sfz);
        assert_eq!(o.binding.region_sample, [Some(0)]);
        let out = root.join("inst/limewave");
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        assert_eq!(std::fs::read(out.join("samples/a.wav")).unwrap(), a);
        reopen_all_found(&out.join("a.sfz"));
        // Saving again from the copy still finds everything.
        let again = open(&out.join("a.sfz"));
        again
            .save(&Destination::Folder(root.join("inst/limewave2")), false)
            .unwrap();
        reopen_all_found(&root.join("inst/limewave2/a.sfz"));
    }

    #[test]
    fn default_path_with_all_samples_outside_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "shared/y.wav", &wav_with(330.0, vec![]));
        put(root, "shared/z.wav", &wav_with(440.0, vec![]));
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<control> default_path=Kit/\n<region> sample=../../shared/y.wav key=60\n<region> sample=../../shared/z.wav key=62\n",
        );
        let o = open(&sfz);
        assert_eq!(o.binding.samples.len(), 2);
        let out = root.join("inst/limewave");
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        let saved = text(&out.join("a.sfz"));
        assert!(saved.contains("sample=../samples/y.wav"), "{saved}");
        // The default folder the text climbs out of exists, for systems that do not fold `..`.
        assert!(out.join("Kit").is_dir());
        reopen_all_found(&out.join("a.sfz"));
    }

    #[test]
    fn folder_refuses_to_write_over_another_samples_original() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let x = wav_with(220.0, vec![]);
        let theirs = wav_with(330.0, vec![]);
        put(root, "inst/x.wav", &x);
        let other = put(root, "inst/limewave/x.wav", &theirs);
        let sfz = put(
            root,
            "inst/a.sfz",
            b"<region> sample=x.wav key=60\n<region> sample=limewave/x.wav key=62\n",
        );
        let o = open(&sfz);
        assert_eq!(o.binding.samples.len(), 2);
        let err = o
            .save(&Destination::Folder(root.join("inst/limewave")), false)
            .unwrap_err();
        assert!(matches!(err, SaveError::WouldOverwriteOriginal(_)), "{err}");
        assert_eq!(std::fs::read(&other).unwrap(), theirs);
        assert!(!root.join("inst/limewave/a.sfz").exists());
        assert!(!root.join("inst/limewave/limewave").exists());
    }

    #[test]
    fn unreadable_copy_source_aborts_with_nothing_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        put(root, "inst/a.wav", &wav_with(220.0, vec![]));
        let pb = put(root, "inst/b.wav", &wav_with(330.0, vec![]));
        let sfz = put(
            root,
            "inst/i.sfz",
            b"<region> sample=a.wav key=60\n<region> sample=b.wav key=62\n",
        );
        let out = root.join("out");
        let old_sfz = put(&out, "i.sfz", b"old");
        let old_a = put(&out, "a.wav", b"old a");
        let mut o = open(&sfz);
        // The first sample is rebuilt; the second is to be copied, and has vanished.
        o.edits[0].end = 2999;
        std::fs::remove_file(&pb).unwrap();
        let err = o
            .save(&Destination::Folder(out.clone()), false)
            .unwrap_err();
        assert!(matches!(err, SaveError::Io(_)), "{err}");
        assert_eq!(std::fs::read(&old_sfz).unwrap(), b"old");
        assert_eq!(std::fs::read(&old_a).unwrap(), b"old a");
        assert!(!out.join("i.sfz.bak").exists());
        assert!(!out.join("a.wav.bak").exists());
        assert!(!out.join("b.wav").exists());
    }

    #[test]
    fn folder_into_an_existing_folder_backs_up_what_it_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let x = wav_with(220.0, vec![]);
        put(root, "inst/x.wav", &x);
        let sfz = put(root, "inst/a.sfz", b"<region> sample=x.wav\n");
        let out = root.join("inst/limewave");
        let o = open(&sfz);
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        let first_sfz = std::fs::read(out.join("a.sfz")).unwrap();

        let saved = o.save(&Destination::Folder(out.clone()), false).unwrap();
        assert_eq!(saved.written.len(), 2);
        assert!(saved.written.iter().all(|w| w.backup.is_some()));
        assert_eq!(std::fs::read(out.join("a.sfz.bak")).unwrap(), first_sfz);
        assert_eq!(std::fs::read(out.join("x.wav.bak")).unwrap(), x);
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

    fn data_of(bytes: &[u8]) -> Vec<u8> {
        let chunks = parse_riff(bytes).unwrap().chunks;
        chunks
            .iter()
            .find(|c| &c.id == b"data")
            .map(|c| c.data.clone())
            .unwrap()
    }

    fn smpl_of(p: &Path) -> Option<Smpl> {
        let chunks = parse_riff(&std::fs::read(p).unwrap()).unwrap().chunks;
        chunks
            .iter()
            .find(|c| &c.id == b"smpl")
            .map(|c| Smpl::parse(&c.data).unwrap())
    }

    /// With `smpl` off, the crossfade is still written, at the loop as edited, while the
    /// `smpl` chunk keeps the WAV's own values (spec 7.3).
    #[test]
    fn smpl_off_writes_the_crossfade_at_the_edited_loop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // (a) A WAV without `smpl`, given a loop in the editor.
        put(root, "a/x.wav", &wav_with(220.0, vec![]));
        let sfz = put(root, "a/a.sfz", b"<region> sample=x.wav\n");
        let mut o = open(&sfz);
        o.edits[0] = EditState {
            crossfade: 100,
            ..looped(&o.initial[0])
        };
        let out = root.join("a/out");
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        let saved = std::fs::read(out.join("x.wav")).unwrap();
        let want = data_of(&build_wav_bytes(&o.docs[0], &o.edits[0]));
        assert_ne!(
            want,
            o.docs[0].data_chunk(),
            "the crossfade changes the audio"
        );
        assert_eq!(data_of(&saved), want);
        assert_eq!(smpl_of(&out.join("x.wav")), None, "no smpl added");

        // (b) A WAV whose own loop differs from the instrument's.
        let mut s = Smpl::parse(&smpl_chunk(60).data).unwrap();
        s.loops = vec![crate::smpl::SmplLoop {
            cue_id: 0,
            loop_type: 0,
            start: 100,
            end: 900,
            fraction: 0,
            play_count: 0,
        }];
        let chunk = Chunk {
            id: *b"smpl",
            data: s.to_bytes(),
        };
        put(root, "b/x.wav", &wav_with(220.0, vec![chunk]));
        let sfz = put(
            root,
            "b/a.sfz",
            b"<region> sample=x.wav loop_mode=loop_continuous loop_start=200 loop_end=800\n",
        );
        let mut o = open(&sfz);
        assert_eq!((o.initial[0].loop_start, o.initial[0].loop_end), (200, 800));
        o.edits[0].crossfade = 100;
        let out = root.join("b/out");
        o.save(&Destination::Folder(out.clone()), false).unwrap();
        let saved = std::fs::read(out.join("x.wav")).unwrap();
        let want = data_of(&build_wav_bytes(&o.docs[0], &o.edits[0]));
        assert_ne!(want, o.docs[0].data_chunk());
        assert_eq!(data_of(&saved), want);
        let l = smpl_of(&out.join("x.wav")).unwrap().loops[0];
        assert_eq!((l.start, l.end), (100, 900), "the WAV's own loop");
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
