//! Opening and closing SFZ instruments through the whole app.

use std::path::{Path, PathBuf};
use std::time::Duration;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use limewave::app::{FileEntry, LimewaveApp, PendingAction};
use limewave_core::edit::EditState;
use limewave_core::riff::Chunk;
use limewave_core::smpl::Smpl;
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

/// A 16-bit mono sine WAV, with a `smpl` chunk of root key `root` when given.
fn write_wav(dir: &Path, name: &str, root: Option<u32>) -> PathBuf {
    let chunks = root
        .map(|unity_note| {
            let smpl = Smpl {
                manufacturer: 0,
                product: 0,
                sample_period: 22_675,
                unity_note,
                pitch_fraction: 0,
                smpte_format: 0,
                smpte_offset: 0,
                loops: Vec::new(),
                sampler_data: Vec::new(),
            };
            vec![Chunk {
                id: *b"smpl",
                data: smpl.to_bytes(),
            }]
        })
        .unwrap_or_default();
    let bytes = wav_bytes(
        &[sine(440.0, 44100, 4410, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        chunks,
    );
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A folder holding `inst.sfz` with `sfz` as its text, `a.wav` (smpl root 60) and `b.wav`.
fn folder(sfz: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    write_wav(dir.path(), "a.wav", Some(60));
    write_wav(dir.path(), "b.wav", None);
    let path = dir.path().join("inst.sfz");
    std::fs::write(&path, sfz).unwrap();
    (dir, path)
}

fn harness(app: LimewaveApp) -> Harness<'static, LimewaveApp> {
    Harness::builder()
        .with_size(eframe::egui::vec2(1100.0, 700.0))
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

/// Opens the instrument at `path` in a fresh app and waits for its samples.
fn open(path: &Path) -> Harness<'static, LimewaveApp> {
    let mut app = LimewaveApp::for_test();
    app.open_instrument(path.to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    h
}

fn file_names(app: &LimewaveApp) -> Vec<String> {
    app.files.iter().map(FileEntry::file_name).collect()
}

#[test]
fn opening_an_instrument_loads_each_wav_once() {
    let (_dir, path) = folder(
        "<region> sample=a.wav lokey=0 hikey=60\n<region> sample=a.wav lokey=61 hikey=127\n",
    );
    let h = open(&path);
    let app = h.state();
    let st = app.instrument.as_ref().expect("the instrument opened");
    assert_eq!(app.files.len(), 1);
    assert!(matches!(app.files[0], FileEntry::Loaded(_)));
    assert_eq!(st.binding.samples.len(), 1);
    assert_eq!(st.binding.samples[0].regions, [0, 1]);
    assert_eq!(st.sample_file, [Some(0)]);
    assert_eq!((st.region_file(0), st.region_file(1)), (Some(0), Some(0)));
    assert_eq!(st.selected_region, Some(0));
    assert_eq!(app.selected, Some(0));
}

#[test]
fn instrument_tuning_wins_and_is_not_an_edit() {
    let (_dir, path) = folder("<region> sample=a.wav pitch_keycenter=49 tune=14\n");
    let h = open(&path);
    let app = h.state();
    let FileEntry::Loaded(s) = &app.files[0] else {
        panic!("a.wav did not load");
    };
    assert_eq!(s.doc.initial_edit().root, 60, "the WAV says 60");
    assert_eq!(s.edit().root, 49);
    assert_eq!(s.edit().cents, -14.0);
    assert!(!s.unsaved());
    assert_eq!(app.dirty_count(), 0);
}

#[test]
fn missing_samples_show_as_failed() {
    let (_dir, path) = folder(
        "<region> sample=a.wav\n<region> sample=gone.wav\n<region> sample=Gone.WAV\n<region> sample=b.wav\n",
    );
    let mut h = open(&path);
    let app = h.state();
    assert_eq!(file_names(app), ["a.wav", "b.wav", "gone.wav"]);
    assert!(matches!(&app.files[2], FileEntry::Failed(_, e) if e == "missing"));
    let st = app.instrument.as_ref().unwrap();
    assert_eq!(st.region_file(1), None);
    assert_eq!(st.region_file(3), Some(1));
    assert_eq!(h.query_all_by_label("failed").count(), 1);

    // A missing region can be selected, but leaves the editor empty.
    h.state_mut().select_region(1);
    assert_eq!(h.state().selected, None);
    assert_eq!(
        h.state().instrument.as_ref().unwrap().selected_region,
        Some(1)
    );
    h.run();
    h.state_mut().select_region(3);
    assert_eq!(h.state().selected, Some(1));
}

#[test]
fn all_missing_opens_without_panic() {
    let (_dir, path) = folder("<region> sample=nope.wav\n<region> sample=*sine\n<region>\n");
    let mut h = open(&path);
    h.run();
    let app = h.state();
    let st = app.instrument.as_ref().expect("the instrument opened");
    assert_eq!(st.inst().regions().len(), 3);
    assert!(st.binding.samples.is_empty());
    assert_eq!(app.files.len(), 2, "nope.wav and *sine are missing");
    assert!(
        app.files
            .iter()
            .all(|f| matches!(f, FileEntry::Failed(_, e) if e == "missing"))
    );
    assert_eq!(st.selected_region, None);
    assert_eq!(app.selected, None);
}

#[test]
fn include_is_refused_with_a_message() {
    let (dir, _) = folder("#include \"other.sfz\"\n<region> sample=a.wav\n");
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    h.get_by_label("inst.sfz").click();
    h.run();
    let app = h.state();
    assert!(app.instrument.is_none());
    let text = "This instrument uses #include, which Limewave can't open yet";
    assert_eq!(app.ui.message.as_deref(), Some(text));
    assert_eq!(file_names(app), ["a.wav", "b.wav"], "the folder stays open");
    h.get_by_label(text);
}

#[test]
fn folder_lists_sfz_files_and_clicking_opens() {
    let (dir, path) = folder("<region> sample=b.wav\n");
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    assert_eq!(app.folder_instruments, std::slice::from_ref(&path));
    let mut h = harness(app);
    wait_loaded(&mut h);
    assert_eq!(file_names(h.state()), ["a.wav", "b.wav"]);
    h.get_by_label("inst.sfz").click();
    wait_loaded(&mut h);
    let app = h.state();
    let st = app
        .instrument
        .as_ref()
        .expect("clicking opened the instrument");
    assert_eq!(st.path, path);
    assert_eq!(file_names(app), ["b.wav"]);
    assert_eq!(app.selected, Some(0));
}

#[test]
fn close_returns_to_the_folder() {
    let (dir, path) = folder("<region> sample=b.wav\n");
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    app.request(PendingAction::OpenInstrument(path.clone()));
    let mut h = harness(app);
    wait_loaded(&mut h);
    assert_eq!(
        h.state().instrument.as_ref().unwrap().return_to,
        Some(PendingAction::OpenFolder(dir.path().to_path_buf()))
    );
    h.state_mut().request(PendingAction::CloseInstrument);
    wait_loaded(&mut h);
    let app = h.state();
    assert!(app.instrument.is_none());
    assert_eq!(file_names(app), ["a.wav", "b.wav"]);
    assert_eq!(app.folder_instruments, [path]);
}

#[test]
fn close_returns_to_the_file_or_to_nothing() {
    let (dir, path) = folder("<region> sample=b.wav\n");
    let mut app = LimewaveApp::for_test();
    app.open_file(dir.path().join("a.wav"));
    app.request(PendingAction::OpenInstrument(path.clone()));
    let mut h = harness(app);
    wait_loaded(&mut h);
    h.state_mut().request(PendingAction::CloseInstrument);
    h.run();
    assert_eq!(file_names(h.state()), ["a.wav"]);

    let mut h = open(&path);
    h.state_mut().request(PendingAction::CloseInstrument);
    h.run();
    assert!(h.state().instrument.is_none());
    assert!(h.state().files.is_empty());
    assert_eq!(h.state().selected, None);
}

#[test]
fn opening_with_unsaved_edits_asks_first() {
    let (dir, path) = folder("<region> sample=b.wav\n");
    let mut app = LimewaveApp::for_test();
    app.open_file(dir.path().join("a.wav"));
    let FileEntry::Loaded(s) = &mut app.files[0] else {
        panic!("a.wav did not load");
    };
    let e = EditState {
        start: 100,
        ..s.edit().clone()
    };
    s.apply(e, None);
    app.request(PendingAction::OpenInstrument(path.clone()));
    assert_eq!(
        app.pending,
        Some(PendingAction::OpenInstrument(path.clone()))
    );
    assert!(app.instrument.is_none());
    app.resolve_pending(true);
    assert!(app.instrument.is_some());

    // The instrument's own edits count as unsaved and hold back Close.
    let mut h = harness(app);
    wait_loaded(&mut h);
    assert_eq!(h.state().dirty_count(), 0);
    let st = h.state_mut().instrument.as_mut().unwrap();
    st.edit(|i| i.set(0, "volume", "-3"));
    assert!(st.history.is_dirty());
    assert_eq!(st.sample_file, [Some(0)], "the edit keeps the binding");
    assert_eq!(h.state().dirty_count(), 1);
    assert!(
        h.state()
            .instrument
            .as_ref()
            .unwrap()
            .unsaved(&h.state().files)
    );
    h.state_mut().request(PendingAction::CloseInstrument);
    assert_eq!(h.state().pending, Some(PendingAction::CloseInstrument));
    h.run();
    h.get_by_label("Unsaved changes");
    h.state_mut().resolve_pending(true);
    assert!(h.state().instrument.is_none());
}

#[test]
fn rsb_library_opens_if_present() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/RSB 501-01");
    if !dir.is_dir() {
        return;
    }
    for (name, regions, samples) in [
        ("E.Piano 1 mp.sfz", 10, Some(8)),
        ("E.Piano 1 vm.sfz", 20, None),
        ("E.Piano 8vb.sfz", 20, None),
    ] {
        let h = open(&dir.join(name));
        let app = h.state();
        let st = app.instrument.as_ref().expect(name);
        assert_eq!(st.inst().regions().len(), regions, "{name}");
        assert!(
            st.binding.region_sample.iter().all(Option::is_some),
            "{name}: a sample is missing"
        );
        assert!(
            app.files.iter().all(|f| matches!(f, FileEntry::Loaded(_))),
            "{name}: a sample did not load"
        );
        if let Some(n) = samples {
            assert_eq!(st.binding.samples.len(), n, "{name}");
        }
        assert_eq!(app.files.len(), st.binding.samples.len(), "{name}");
        assert!(app.selected.is_some(), "{name}");
    }
}
