//! Saving an SFZ instrument from the app: the save dialog's SFZ format, Save, Save all and
//! the untitled instrument (spec 7).

use std::path::{Path, PathBuf};
use std::time::Duration;

use eframe::egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, PendingAction, Session};
use limewave::audio::voice::Playback;
use limewave::instrument::InstrumentState;
use limewave::ui::region_inspector::RegionEdit;
use limewave::ui::save_dialog::{InstrumentSaveRequest, SaveFormat};
use limewave_core::document::{load_wav, new_document};
use limewave_core::edit::{EditState, LoopMode, resample_edit};
use limewave_core::effects::{resample_map, resample_map_end};
use limewave_core::riff::Chunk;
use limewave_core::save::Destination;
use limewave_core::smpl::Smpl;
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

/// A 16-bit mono sine WAV with a `smpl` chunk of root key 60.
fn write_wav(path: &Path) {
    let smpl = Smpl {
        manufacturer: 0,
        product: 0,
        sample_period: 22_675,
        unity_note: 60,
        pitch_fraction: 0,
        smpte_format: 0,
        smpte_offset: 0,
        loops: Vec::new(),
        sampler_data: Vec::new(),
    };
    let bytes = wav_bytes(
        &[sine(440.0, 44100, 4410, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        vec![Chunk {
            id: *b"smpl",
            data: smpl.to_bytes(),
        }],
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// The instrument text of [`kit`]: one sample named with a backslash, one with a slash, and
/// one outside the instrument's folder.
const KIT: &str = "<region> sample=Samples\\a.wav lokey=0 hikey=59\n\
<region> sample=Samples/b.wav lokey=60 hikey=80\n\
<region> sample=../outside.wav lokey=81 hikey=127\n";

/// `root/kit/kit.sfz` playing `root/kit/Samples/a.wav`, `root/kit/Samples/b.wav` and
/// `root/outside.wav`. Returns the temporary root and the `.sfz` path.
fn kit() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let kit = dir.path().join("kit");
    write_wav(&kit.join("Samples").join("a.wav"));
    write_wav(&kit.join("Samples").join("b.wav"));
    write_wav(&dir.path().join("outside.wav"));
    let path = kit.join("kit.sfz");
    std::fs::write(&path, KIT).unwrap();
    (dir, path)
}

fn harness(app: LimewaveApp) -> Harness<'static, LimewaveApp> {
    Harness::builder()
        .with_size(vec2(1280.0, 900.0))
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app)
}

/// Steps until no file is still loading.
fn wait_loaded(h: &mut Harness<'static, LimewaveApp>) {
    for _ in 0..800 {
        h.step();
        let done = h
            .state()
            .files
            .iter()
            .all(|f| !matches!(f, FileEntry::Loading(_)));
        if done {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run();
}

/// Steps until the save dialog has grown or shrunk to its new content: it resizes over
/// several frames, and a click lands where its widgets are drawn.
fn settle(h: &mut Harness<'static, LimewaveApp>) {
    for _ in 0..20 {
        h.step();
    }
    h.run();
}

/// Opens the instrument at `path` in a fresh app (no audio engine) and waits for its samples.
fn open(path: &Path) -> Harness<'static, LimewaveApp> {
    let mut app = LimewaveApp::for_test();
    app.open_instrument(path.to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    h
}

fn st<'a>(h: &'a Harness<'static, LimewaveApp>) -> &'a InstrumentState {
    h.state()
        .instrument
        .as_ref()
        .expect("an instrument is open")
}

fn st_mut<'a>(h: &'a mut Harness<'static, LimewaveApp>) -> &'a mut InstrumentState {
    h.state_mut()
        .instrument
        .as_mut()
        .expect("an instrument is open")
}

/// The loaded session that `region` plays.
fn region_session<'a>(h: &'a mut Harness<'static, LimewaveApp>, region: usize) -> &'a mut Session {
    let i = st(h).region_file(region).expect("the region has a file");
    match &mut h.state_mut().files[i] {
        FileEntry::Loaded(s) => s,
        _ => panic!("region {region}'s sample is not loaded"),
    }
}

/// Applies `f` to the edit of the sample `region` plays, as one undo step.
fn edit_sample(
    h: &mut Harness<'static, LimewaveApp>,
    region: usize,
    f: impl FnOnce(&mut EditState),
) {
    let s = region_session(h, region);
    let mut e = s.edit().clone();
    f(&mut e);
    s.apply(e, None);
}

/// Every file under `dir`, as paths relative to it with `/`, sorted.
fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path.strip_prefix(base).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn backups(dir: &Path) -> Vec<String> {
    tree(dir)
        .into_iter()
        .filter(|p| p.ends_with(".bak"))
        .collect()
}

#[test]
fn save_instrument_to_new_folder_writes_sfz_and_wavs_and_reopens() {
    let (dir, path) = kit();
    let kit_dir = path.parent().unwrap().to_path_buf();
    let mut h = open(&path);
    st_mut(&mut h).edit(|i| i.set(1, "volume", "-6"));
    edit_sample(&mut h, 0, |e| e.root = 64);
    h.state_mut().select_region(1);
    h.state_mut().save_as();
    settle(&mut h);
    let d = &h.state().ui.save_dialog;
    assert!(d.open && d.instrument && !d.overwrite);
    assert_eq!(d.format, SaveFormat::Sfz);
    assert_eq!(d.folder, kit_dir.join("limewave"));
    h.get_by_label("Save instrument").click();
    h.run();
    wait_loaded(&mut h);

    let out = kit_dir.join("limewave");
    // `samples/` is `Samples/` where file names ignore case.
    let mut saved: Vec<String> = tree(&out).iter().map(|p| p.to_lowercase()).collect();
    saved.sort();
    assert_eq!(
        saved,
        [
            "kit.sfz",
            "samples/a.wav",
            "samples/b.wav",
            "samples/outside.wav"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        KIT,
        "the original is untouched"
    );
    let app = h.state();
    let s = st(&h);
    assert_eq!(s.path, out.join("kit.sfz"));
    assert!(!s.untitled);
    assert_eq!(s.destination, Some(Destination::ReplaceSaved));
    assert!(s.return_to.is_none(), "inherited from the instrument saved");
    // Every region finds its sample in the saved folder.
    for r in 0..3 {
        let i = s
            .region_file(r)
            .unwrap_or_else(|| panic!("region {r} has no file"));
        let FileEntry::Loaded(session) = &app.files[i] else {
            panic!("region {r}'s sample did not load");
        };
        assert!(session.doc.path.starts_with(&out), "{:?}", session.doc.path);
    }
    assert_eq!(app.dirty_count(), 0);
    assert_eq!(s.selected_region, Some(1), "the same region is selected");
    assert_eq!(app.selected, s.region_file(1));
    assert_eq!(s.inst().region_info(1).volume, -6.0);
    assert_eq!(s.inst().get(0, "pitch_keycenter").unwrap().0, "64");
    assert_eq!(s.inst().get(2, "sample").unwrap().0, "samples/outside.wav");
    assert!(
        s.history.cursor() == 0 && !s.history.is_dirty(),
        "a fresh history"
    );
    let results = app
        .save_results
        .as_ref()
        .expect("the dialog shows what it saved");
    assert!(results.iter().all(|(_, r)| r.is_ok()), "{results:?}");
    let _ = dir;
}

#[test]
fn second_save_replaces_without_backup() {
    let (_dir, path) = kit();
    let out = path.parent().unwrap().join("limewave");
    let mut h = open(&path);
    st_mut(&mut h).edit(|i| i.set(1, "volume", "-6"));
    h.state_mut()
        .save_instrument_now(InstrumentSaveRequest {
            destination: Destination::Folder(out.clone()),
            write_smpl: false,
        })
        .unwrap();
    wait_loaded(&mut h);
    let before = tree(&out);

    st_mut(&mut h).edit(|i| i.set(1, "volume", "-12"));
    edit_sample(&mut h, 0, |e| e.start = 100);
    assert!(h.state().dirty_count() > 0);
    h.state_mut().save();
    wait_loaded(&mut h);
    assert!(
        !h.state().ui.save_dialog.open,
        "Save repeats the last choice"
    );
    assert!(
        h.state().save_results.is_none(),
        "{:?}",
        h.state().save_results
    );
    assert_eq!(tree(&out), before, "no backups, nothing new");
    assert!(backups(&out).is_empty());
    let text = std::fs::read_to_string(out.join("kit.sfz")).unwrap();
    assert!(text.contains("volume=-12"), "{text}");
    assert_eq!(
        load_wav(&out.join("Samples").join("a.wav")).unwrap().frames,
        4410 - 100
    );
    assert_eq!(st(&h).path, out.join("kit.sfz"));
    assert_eq!(h.state().dirty_count(), 0);
}

#[test]
fn overwrite_makes_bak_files() {
    let (dir, path) = kit();
    let kit_dir = path.parent().unwrap().to_path_buf();
    let mut h = open(&path);
    st_mut(&mut h).edit(|i| i.set(1, "volume", "-6"));
    edit_sample(&mut h, 0, |e| e.start = 100);
    h.state_mut().save_as();
    h.state_mut().ui.save_dialog.overwrite = true;
    settle(&mut h);
    h.get_by_label("Save instrument").click();
    h.run();
    wait_loaded(&mut h);

    assert_eq!(
        backups(dir.path()),
        ["kit/Samples/a.wav.bak", "kit/kit.sfz.bak"],
        "unchanged WAVs are left alone"
    );
    assert_eq!(
        std::fs::read_to_string(kit_dir.join("kit.sfz.bak")).unwrap(),
        KIT
    );
    assert!(!kit_dir.join("limewave").exists());
    assert_eq!(st(&h).path, path);
    assert_eq!(st(&h).destination, Some(Destination::OverwriteWithBackup));
    assert_eq!(h.state().dirty_count(), 0);
    assert_eq!(
        load_wav(&kit_dir.join("Samples").join("a.wav"))
            .unwrap()
            .frames,
        4310
    );

    // Save repeats the overwrite, with a new numbered backup.
    st_mut(&mut h).edit(|i| i.set(1, "volume", "-12"));
    h.state_mut().save();
    wait_loaded(&mut h);
    assert!(kit_dir.join("kit.sfz.1.bak").exists());
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("volume=-12")
    );
}

#[test]
fn smpl_checkbox_controls_wav_root() {
    for write_smpl in [false, true] {
        let (_dir, path) = kit();
        let out = path.parent().unwrap().join("limewave");
        let mut h = open(&path);
        edit_sample(&mut h, 0, |e| e.root = 64);
        h.state_mut().save_as();
        assert!(!h.state().ui.save_dialog.write_smpl, "off by default");
        settle(&mut h);
        if write_smpl {
            h.get_by_label("Also write loop and root key into each WAV (smpl)")
                .click();
            settle(&mut h);
        }
        h.get_by_label("Save instrument").click();
        h.run();
        wait_loaded(&mut h);

        let saved = load_wav(&out.join("Samples").join("a.wav")).unwrap();
        let wav_root = if write_smpl { 64 } else { 60 };
        assert_eq!(
            saved.initial_edit().root,
            wav_root,
            "write_smpl {write_smpl}"
        );
        let text = std::fs::read_to_string(out.join("kit.sfz")).unwrap();
        assert!(text.contains("pitch_keycenter=64"), "{text}");
        assert_eq!(st(&h).write_smpl, write_smpl, "remembered");
        assert_eq!(region_session(&mut h, 0).edit().root, 64);
    }
}

/// A folder with `a.wav` and `b.wav`, opened, with a new untitled instrument made from them.
fn untitled() -> (tempfile::TempDir, Harness<'static, LimewaveApp>) {
    let dir = tempfile::tempdir().unwrap();
    write_wav(&dir.path().join("a.wav"));
    write_wav(&dir.path().join("b.wav"));
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    h.state_mut().new_instrument();
    h.run();
    assert!(st(&h).untitled);
    (dir, h)
}

#[test]
fn untitled_instrument_saves_through_picker_with_sfz_added() {
    for (picked, saved) in [
        ("My kit", "My kit.sfz"),
        ("kit.v1", "kit.v1.sfz"),
        ("Loud.SFZ", "Loud.SFZ"),
    ] {
        let (dir, mut h) = untitled();
        let target = dir.path().join("out").join(picked);
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = asked.clone();
        h.state_mut().save_picker = Box::new(move |name| {
            seen.borrow_mut().push(name.to_owned());
            Some(target.clone())
        });
        h.state_mut().save();
        wait_loaded(&mut h);
        assert_eq!(*asked.borrow(), ["Untitled instrument.sfz"]);

        let out = dir.path().join("out");
        let mut want = vec!["a.wav", "b.wav", saved];
        want.sort_unstable();
        assert_eq!(tree(&out), want, "{picked}");
        let s = st(&h);
        assert_eq!(s.path, out.join(saved));
        assert!(!s.untitled);
        assert_eq!(s.destination, Some(Destination::ReplaceSaved));
        assert!(matches!(
            &s.return_to,
            Some(PendingAction::OpenFolder(d)) if d == dir.path()
        ));
        assert_eq!(h.state().dirty_count(), 0);
        assert!(h.state().save_results.is_none());
    }

    // The next Save writes over the saved copy without asking.
    let (dir, mut h) = untitled();
    let target = dir.path().join("out").join("kit");
    h.state_mut().save_picker = Box::new(move |_| Some(target.clone()));
    h.state_mut().save_as();
    wait_loaded(&mut h);
    st_mut(&mut h).edit(|i| i.set(0, "volume", "-3"));
    h.state_mut().save_picker = Box::new(|_| panic!("a saved instrument does not ask"));
    h.state_mut().save();
    wait_loaded(&mut h);
    let out = dir.path().join("out");
    assert!(backups(&out).is_empty());
    assert!(
        std::fs::read_to_string(out.join("kit.sfz"))
            .unwrap()
            .contains("volume=-3")
    );
}

#[test]
fn wav_format_saves_samples_and_leaves_instrument_unsaved() {
    let (_dir, path) = kit();
    let kit_dir = path.parent().unwrap().to_path_buf();
    let mut h = open(&path);
    st_mut(&mut h).edit(|i| i.set(1, "volume", "-6"));
    edit_sample(&mut h, 0, |e| e.start = 100);
    h.state_mut().save_as();
    settle(&mut h);
    h.get_by_label("Individual WAV files").click();
    settle(&mut h);
    assert_eq!(h.state().ui.save_dialog.format, SaveFormat::Wav);
    h.get_by_label("Save 1 file(s)").click();
    h.run();

    // Into the dialog's folder, as for a folder of WAVs; no instrument is written.
    let out = kit_dir.join("limewave");
    assert_eq!(tree(&out), ["a.wav"]);
    assert_eq!(load_wav(&out.join("a.wav")).unwrap().frames, 4310);
    let results = h.state().save_results.as_ref().unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_ok(), "{results:?}");
    let s = st(&h);
    assert_eq!(s.path, path);
    assert!(s.history.is_dirty(), "the instrument stays unsaved");
    assert!(h.state().dirty_count() > 0);
}

#[test]
fn sfz_option_disabled_without_instrument() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    write_wav(&wav);
    let mut app = LimewaveApp::for_test();
    app.open_file(wav);
    app.save_as();
    let mut h = harness(app);
    h.run();
    assert!(h.state().ui.save_dialog.open);
    assert_eq!(h.state().ui.save_dialog.format, SaveFormat::Wav);
    assert!(
        h.get_by_label("SFZ instrument + its WAVs")
            .accesskit_node()
            .is_disabled()
    );
    h.get_by_label("open an instrument first");
    assert!(
        h.query_by_label("Both").is_none(),
        "the Both choice is gone"
    );
    assert!(h.query_by_label("Save instrument").is_none());
    h.get_by_label("Save 1 file(s)");

    // With an instrument open the SFZ choice is live and hides the file table.
    let (_kit_dir, path) = kit();
    let mut h = open(&path);
    h.state_mut().save_as();
    settle(&mut h);
    assert!(
        !h.get_by_label("SFZ instrument + its WAVs")
            .accesskit_node()
            .is_disabled()
    );
    assert!(h.query_by_label("open an instrument first").is_none());
    assert!(h.query_by_label("Only save files that changed").is_none());
    h.get_by_label("Save instrument");

    // A file outside the open instrument gets the plain WAV dialog, which says how to save
    // the instrument instead.
    let extra = _kit_dir.path().join("extra.wav");
    write_wav(&extra);
    let app = h.state_mut();
    app.ui.save_dialog.open = false;
    app.files.push(FileEntry::Loaded(Box::new(Session::new(
        load_wav(&extra).unwrap(),
    ))));
    app.select(app.files.len() - 1);
    app.save_as();
    settle(&mut h);
    assert!(!h.state().ui.save_dialog.instrument);
    h.get_by_label("select one of the instrument's regions first");
    assert!(h.query_by_label("open an instrument first").is_none());
}

#[test]
fn save_all_from_unsaved_prompt_saves_instrument() {
    let (dir, path) = kit();
    let kit_dir = path.parent().unwrap().to_path_buf();
    let mut h = open(&path);
    st_mut(&mut h).edit(|i| i.set(1, "volume", "-6"));
    edit_sample(&mut h, 0, |e| e.start = 100);
    // An untitled recording is not one of the instrument's samples: it is saved on its own.
    let name = h.state_mut().next_name(limewave::app::NewName::Recording);
    h.state_mut().add_untitled(new_document(
        name,
        vec![vec![0.0; 100]],
        44_100,
        SampleFormat::Int { bits: 16 },
        60,
        false,
    ));
    let recording = dir.path().join("rec.wav");
    let target = recording.clone();
    h.state_mut().save_picker = Box::new(move |name| {
        assert!(name.ends_with(".wav"), "only the recording asks: {name}");
        Some(target.clone())
    });
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    h.state_mut()
        .request(PendingAction::OpenFolder(empty.clone()));
    h.run();
    h.get_by_label("1 instrument and 1 file(s) are not saved. Discarding them cannot be undone.");
    h.get_by_label("Save all").click();
    h.run();

    let app = h.state();
    assert!(app.save_results.is_none(), "{:?}", app.save_results);
    assert!(app.pending.is_none());
    assert!(app.instrument.is_none(), "the held action ran");
    assert_eq!(app.folder.as_deref(), Some(empty.as_path()));
    let out = kit_dir.join("limewave");
    assert!(
        std::fs::read_to_string(out.join("kit.sfz"))
            .unwrap()
            .contains("volume=-6")
    );
    assert_eq!(
        load_wav(&out.join("Samples").join("a.wav")).unwrap().frames,
        4310
    );
    assert!(
        !kit_dir.join("Samples").join("limewave").exists(),
        "the samples were not saved one by one"
    );
    assert!(recording.exists());

    // An untitled instrument whose picker is cancelled keeps the action from running.
    let (dir, mut h) = untitled();
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    h.state_mut().request(PendingAction::OpenFolder(empty));
    h.run();
    h.get_by_label("1 instrument is not saved. Discarding it cannot be undone.");
    h.state_mut().resolve_pending_save_all();
    h.run();
    assert!(st(&h).untitled, "the held action did not run");
    assert_eq!(h.state().files.len(), 2);
    assert!(h.state().save_results.is_none());
}

#[test]
fn wav_export_leaves_instrument_and_samples_unsaved() {
    let (dir, path) = kit();
    let kit_dir = path.parent().unwrap().to_path_buf();
    let a = kit_dir.join("Samples").join("a.wav");
    let mut h = open(&path);
    // Sample edits only: the instrument's history does not move.
    edit_sample(&mut h, 0, |e| {
        e.start = 100;
        e.root = 64;
    });
    h.state_mut().save_as();
    settle(&mut h);
    h.get_by_label("Individual WAV files").click();
    settle(&mut h);
    assert!(
        h.get_by_label("Overwrite originals")
            .accesskit_node()
            .is_disabled(),
        "an export never replaces the samples"
    );
    h.get_by_label("Save 1 file(s)").click();
    h.run();

    let exported = kit_dir.join("limewave").join("a.wav");
    let results = h.state().save_results.clone().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, a);
    assert_eq!(results[0].1.as_ref().unwrap().written, exported);
    assert_eq!(load_wav(&exported).unwrap().frames, 4310);
    let s = region_session(&mut h, 0);
    assert_eq!(s.doc.path, a, "the session stays on its own file");
    assert!(s.unsaved());
    assert_eq!((s.edit().start, s.edit().root), (100, 64));
    let app = h.state();
    assert!(st(&h).unsaved(&app.files));
    assert!(app.dirty_count() > 0);

    // The instrument still has the edits to save.
    let copy = dir.path().join("copy");
    h.state_mut()
        .save_instrument_now(InstrumentSaveRequest {
            destination: Destination::Folder(copy.clone()),
            write_smpl: false,
        })
        .unwrap();
    wait_loaded(&mut h);
    assert_eq!(
        load_wav(&copy.join("Samples").join("a.wav"))
            .unwrap()
            .frames,
        4310
    );
    let text = std::fs::read_to_string(copy.join("kit.sfz")).unwrap();
    assert!(text.contains("pitch_keycenter=64"), "{text}");
    assert_eq!(h.state().dirty_count(), 0);
}

#[test]
fn untitled_saved_beside_its_wavs_keeps_backups() {
    let (dir, mut h) = untitled();
    let target = dir.path().join("kit");
    h.state_mut().save_picker = Box::new(move |_| Some(target.clone()));
    h.state_mut().save();
    wait_loaded(&mut h);
    assert_eq!(st(&h).path, dir.path().join("kit.sfz"));
    assert_eq!(
        st(&h).destination,
        Some(Destination::OverwriteWithBackup),
        "the saved instrument plays the original WAVs"
    );
    let original = std::fs::read(dir.path().join("a.wav")).unwrap();

    edit_sample(&mut h, 0, |e| e.start = 100);
    h.state_mut().save_picker = Box::new(|_| panic!("a saved instrument does not ask"));
    h.state_mut().save();
    wait_loaded(&mut h);
    assert!(h.state().save_results.is_none());
    assert_eq!(
        std::fs::read(dir.path().join("a.wav.bak")).unwrap(),
        original
    );
    assert!(dir.path().join("kit.sfz.bak").exists());
    assert_eq!(load_wav(&dir.path().join("a.wav")).unwrap().frames, 4310);
}

#[test]
fn same_named_samples_block_the_wav_export() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(&dir.path().join("Samples").join("x.wav"));
    write_wav(&dir.path().join("Other").join("X.wav"));
    let path = dir.path().join("kit.sfz");
    std::fs::write(
        &path,
        "<region> sample=Samples/x.wav hikey=60\n<region> sample=Other/X.wav lokey=61\n",
    )
    .unwrap();
    let mut h = open(&path);
    for r in [0, 1] {
        edit_sample(&mut h, r, |e| e.start = 100);
    }
    h.state_mut().save_as();
    h.state_mut().ui.save_dialog.format = SaveFormat::Wav;
    settle(&mut h);
    let app = h.state();
    let d = &app.ui.save_dialog;
    assert_eq!(d.chosen(&app.files).len(), 2);
    let names = d.duplicate_names(&app.files);
    assert_eq!(names.len(), 1);
    assert!(names[0].eq_ignore_ascii_case("x.wav"));
    assert_eq!(d.request(&app.files), None);
    h.get_by_label(&format!(
        "More than one chosen file is called {}; they would replace each other in this folder. Save them one at a time.",
        names[0]
    ));
    assert!(
        h.get_by_label("Save 2 file(s)")
            .accesskit_node()
            .is_disabled()
    );

    // One of them alone can be saved.
    let other = st(&h).region_file(1).unwrap();
    h.state_mut().ui.save_dialog.selected_files[other] = false;
    let app = h.state();
    assert!(app.ui.save_dialog.request(&app.files).is_some());
}

#[test]
fn sfz_save_into_a_folder_with_files_asks_first() {
    let (_dir, path) = kit();
    let out = path.parent().unwrap().join("limewave");
    std::fs::create_dir_all(out.join("Samples")).unwrap();
    std::fs::write(out.join("kit.sfz"), "old").unwrap();
    std::fs::write(out.join("Samples").join("b.wav"), "old wav").unwrap();
    let mut h = open(&path);
    st_mut(&mut h).edit(|i| i.set(1, "volume", "-6"));
    h.state_mut().save_as();
    settle(&mut h);
    let app = h.state();
    let mut conflicts = app.ui.save_dialog.conflicts(&app.files);
    conflicts.sort();
    assert_eq!(
        conflicts,
        [out.join("Samples").join("b.wav"), out.join("kit.sfz")]
    );
    assert_eq!(app.ui.save_dialog.request(&app.files), None);
    assert!(
        h.get_by_label("Save instrument")
            .accesskit_node()
            .is_disabled()
    );
    h.get_by_label("Replace existing files").click();
    settle(&mut h);
    h.get_by_label("Save instrument").click();
    h.run();
    wait_loaded(&mut h);

    assert_eq!(
        std::fs::read_to_string(out.join("kit.sfz.bak")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("Samples").join("b.wav.bak")).unwrap(),
        "old wav"
    );
    assert!(
        std::fs::read_to_string(out.join("kit.sfz"))
            .unwrap()
            .contains("volume=-6")
    );
    assert_eq!(st(&h).path, out.join("kit.sfz"));
    assert_eq!(h.state().dirty_count(), 0);
}

#[test]
fn a_failed_save_keeps_the_instrument_and_its_edits() {
    let (_dir, path) = kit();
    let kit_dir = path.parent().unwrap().to_path_buf();
    let mut h = open(&path);
    st_mut(&mut h).edit(|i| i.set(1, "volume", "-6"));
    edit_sample(&mut h, 0, |e| e.start = 100);
    // A folder save into the instrument's own folder would replace the originals.
    let own = Destination::Folder(kit_dir.clone());
    st_mut(&mut h).destination = Some(own.clone());
    h.state_mut().save();
    h.run();

    let results = h
        .state()
        .save_results
        .clone()
        .expect("the failure is shown");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, path);
    assert!(results[0].1.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), KIT);
    let s = st(&h);
    assert_eq!(s.path, path);
    assert_eq!(s.destination, Some(own));
    assert!(s.history.is_dirty());
    assert_eq!(s.inst().region_info(1).volume, -6.0);
    let session = region_session(&mut h, 0);
    assert!(session.history.is_dirty());
    assert_eq!(session.edit().start, 100);
    assert_eq!(session.doc.path, kit_dir.join("Samples").join("a.wav"));
}

#[test]
fn a_one_shot_region_stays_one_shot_through_sample_edits_and_a_save() {
    let (_dir, path) = kit();
    let out = path.parent().unwrap().join("limewave");
    let mut h = open(&path);
    h.state_mut().select_region(0);
    h.state_mut()
        .region_edit(RegionEdit::Playback(Playback::OneShot));
    edit_sample(&mut h, 0, |e| {
        e.root = 62;
        (e.loop_mode, e.loop_start, e.loop_end) = (LoopMode::Forward, 100, 900);
    });
    h.state_mut()
        .save_instrument_now(InstrumentSaveRequest {
            destination: Destination::Folder(out.clone()),
            write_smpl: false,
        })
        .unwrap();
    let text = std::fs::read_to_string(out.join("kit.sfz")).unwrap();
    let first = text.split("<region>").nth(1).unwrap();
    assert!(first.contains("loop_mode=one_shot"), "{text}");
    assert!(!first.contains("loop_continuous"), "{text}");
    assert!(first.contains("pitch_keycenter=62"), "{text}");
    assert!(first.contains("loop_start=100"), "{text}");
}

#[test]
fn a_new_instrument_keeps_each_wavs_fine_tune() {
    let dir = tempfile::tempdir().unwrap();
    let smpl = Smpl {
        manufacturer: 0,
        product: 0,
        sample_period: 22_675,
        unity_note: 60,
        pitch_fraction: 1 << 30,
        smpte_format: 0,
        smpte_offset: 0,
        loops: Vec::new(),
        sampler_data: Vec::new(),
    };
    let bytes = wav_bytes(
        &[sine(440.0, 44100, 4410, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        vec![Chunk {
            id: *b"smpl",
            data: smpl.to_bytes(),
        }],
    );
    std::fs::write(dir.path().join("a.wav"), bytes).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    assert_eq!(region_cents(&h), 25.0, "the WAV is 25 cents sharp");
    h.state_mut().new_instrument();
    h.run();
    assert_eq!(
        st(&h).inst().get(0, "tune").map(|t| t.0).as_deref(),
        Some("-25")
    );
    assert_eq!(st(&h).binding.samples[0].follows, [true]);

    let target = dir.path().join("out").join("kit.sfz");
    h.state_mut().save_picker = Box::new(move |_| Some(target.clone()));
    h.state_mut().save();
    wait_loaded(&mut h);
    assert!(st(&h).path.ends_with("out/kit.sfz"));
    assert_eq!(region_cents(&h), 25.0, "reopened with its fine tune");
}

/// The fine tune of the first loaded file's editor.
fn region_cents(h: &Harness<'static, LimewaveApp>) -> f32 {
    h.state()
        .files
        .iter()
        .find_map(|f| match f {
            FileEntry::Loaded(s) => Some(s.edit().cents),
            _ => None,
        })
        .expect("a loaded file")
}

/// Converts the first RSB 501-01 instrument's first sample to 44,100 Hz the way the rate dialog
/// does, saves the instrument to a new folder and reopens it. The WAV reports the new rate and
/// every region using the sample has its frame opcodes scaled (R3). Skips itself when the
/// library is absent.
#[test]
fn rsb_sample_converts_and_its_regions_scale() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/RSB 501-01");
    if !dir.is_dir() {
        return;
    }
    let mut h = open(&dir.join("E.Piano 1 mp.sfz"));
    h.state_mut().select_region(0);
    let file = st(&h).region_file(0).expect("region 0 has a sample");
    let sample = st(&h).binding.region_sample[0].expect("region 0 has a sample");
    let regions = st(&h).binding.samples[sample].regions.clone();
    let before: Vec<[Option<usize>; 2]> = regions
        .iter()
        .map(|&r| {
            ["loop_start", "loop_end"].map(|n| st(&h).inst().get(r, n)?.0.trim().parse().ok())
        })
        .collect();

    let (e, steps, wav_loop) = {
        let FileEntry::Loaded(s) = &h.state().files[file] else {
            panic!("the sample is not loaded");
        };
        assert_ne!(s.rate(), 44_100, "the library sample is not at 44,100 Hz");
        let e = resample_edit(
            s.edit(),
            &s.processed,
            44_100,
            s.doc.format.sample_rate,
            s.doc.frames,
        )
        .expect("the sample converts");
        let steps = e.effects[s.edit().effects.len()..].to_vec();
        let wav_loop = (s.edit().loop_mode, s.edit().loop_start, s.edit().loop_end);
        (e, steps, wav_loop)
    };
    h.state_mut().apply_rate_edit(e);

    let out = tempfile::tempdir().unwrap();
    h.state_mut().save_as();
    settle(&mut h);
    h.state_mut().ui.save_dialog.folder = out.path().to_path_buf();
    h.get_by_label("Save instrument").click();
    h.run();
    wait_loaded(&mut h);

    let results = h.state().save_results.as_ref().expect("a save result");
    assert!(results.iter().all(|(_, r)| r.is_ok()), "{results:?}");
    let reopened = open(&out.path().join("E.Piano 1 mp.sfz"));
    let sample = st(&reopened).binding.region_sample[0].expect("region 0 has a sample");
    assert_eq!(st(&reopened).binding.samples[sample].regions, regions);
    let i = st(&reopened).region_file(0).unwrap();
    let FileEntry::Loaded(s) = &reopened.state().files[i] else {
        panic!("the saved sample did not load");
    };
    assert_eq!(s.doc.format.sample_rate, 44_100);
    // These instruments set no loop of their own, so the sample's `smpl` loop is what plays.
    let (mode, start, end) = wav_loop;
    eprintln!(
        "DBG {wav_loop:?} {:?}",
        steps
            .iter()
            .map(|a| format!("{:?}", a.op))
            .collect::<Vec<_>>()
    );
    assert_ne!(mode, LoopMode::Off, "the library sample has a loop");
    assert_eq!(
        (s.edit().loop_mode, s.edit().loop_start, s.edit().loop_end),
        (
            mode,
            resample_map(&steps, start),
            resample_map_end(&steps, end)
        )
    );
    for (&r, was) in regions.iter().zip(&before) {
        let get =
            |n: &str| -> Option<usize> { st(&reopened).inst().get(r, n)?.0.trim().parse().ok() };
        assert_eq!(
            get("loop_start"),
            was[0].map(|v| resample_map(&steps, v)),
            "region {r}"
        );
        assert_eq!(
            get("loop_end"),
            was[1].map(|v| resample_map_end(&steps, v)),
            "region {r}"
        );
    }
}
