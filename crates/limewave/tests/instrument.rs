//! Opening and closing SFZ instruments through the whole app.

use std::path::{Path, PathBuf};
use std::time::Duration;

use eframe::egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, PendingAction};
use limewave::audio::voice::{Adsr, Playback};
use limewave::instrument::{InstrumentState, play_map, set_keys};
use limewave::ui::keyboard::key_rect_in;
use limewave::ui::keymap::box_rect;
use limewave::ui::region_inspector::RegionEdit;
use limewave::ui::regions::{RegionListAction, unassigned_wavs, unsaved_regions};
use limewave_core::edit::{EditState, LoopMode};
use limewave_core::riff::Chunk;
use limewave_core::sfz::{Keycenter, Level};
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
    assert_eq!(
        h.query_all_by_label("missing").count(),
        2,
        "the two regions with no file say so"
    );

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

/// Opens `sfz` (with `a.wav`, `b.wav` and `c.wav` beside it) in a harness.
fn open_text(sfz: &str) -> (tempfile::TempDir, Harness<'static, LimewaveApp>) {
    let (dir, path) = folder(sfz);
    write_wav(dir.path(), "c.wav", Some(72));
    let h = open(&path);
    (dir, h)
}

fn st<'a>(h: &'a Harness<'static, LimewaveApp>) -> &'a InstrumentState {
    h.state().instrument.as_ref().unwrap()
}

/// The name of the file the editor shows.
fn shown(app: &LimewaveApp) -> String {
    app.files[app.selected.expect("a file is selected")].file_name()
}

/// The invariant behind Review Focus 4: the selected region exists, and the editor shows its
/// sample, or nothing when it has none loaded.
fn assert_selection_valid(app: &LimewaveApp) {
    let st = app.instrument.as_ref().unwrap();
    let count = st.inst().regions().len();
    match st.selected_region {
        Some(r) => {
            assert!(r < count, "region {r} of {count}");
            assert_eq!(
                app.selected,
                st.region_file(r),
                "the editor follows the region"
            );
        }
        None => assert_eq!(app.selected, None, "no region, no sample"),
    }
    if let Some(i) = app.selected {
        assert!(matches!(app.files[i], FileEntry::Loaded(_)));
    }
}

#[test]
fn rows_are_grouped_and_labelled() {
    let (_dir, mut h) = open_text(
        "<region> sample=b.wav pitch_keycenter=67 lokey=67 hikey=67\n\
         <group> <region> sample=a.wav pitch_keycenter=60 lokey=54 hikey=69 loop_mode=loop_continuous\n\
         <group> <region> sample=gone.wav pitch_keycenter=48 lokey=0 hikey=53\n\
         <group> group_label=Soft lovel=1 hivel=64 <region> sample=a.wav pitch_keycenter=62 lokey=70 hikey=70\n\
         <group> lovel=65 <region> sample=b.wav pitch_keycenter=65 lokey=71 hikey=80\n",
    );
    h.run();
    h.get_by_label("No group");
    h.get_by_label("Group 2");
    h.get_by_label("Group 4 \u{b7} vel 65\u{2013}127");
    h.get_by_label("Soft \u{b7} vel 1\u{2013}64");
    h.get_by_label("C4");
    h.get_by_label("F#3\u{2013}A4");
    // The first a.wav region asks for a loop; the second follows the shared sample into it.
    assert_eq!(h.query_all_by_label("\u{27f2}").count(), 2);
    assert_eq!(h.query_all_by_label("missing").count(), 1);
    assert_eq!(h.query_all_by_label("a.wav").count(), 2);
    h.get_by_label("gone.wav");
    h.get_by_label("5 regions \u{b7} 4 groups");
    // A one-note range shows the note alone.
    h.get_by_label("A#4");
    h.get_by_label("Group 1");
}

#[test]
fn group_one_is_titled_when_it_comes_first() {
    let (_dir, mut h) = open_text("<group> <region> sample=a.wav\n<group> <region> sample=b.wav\n");
    h.run();
    h.get_by_label("Group 1");
    h.get_by_label("Group 2");
    assert!(h.query_by_label("No group").is_none());
}

#[test]
fn clicking_a_row_selects_region_and_sample() {
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav pitch_keycenter=60 lokey=0 hikey=62\n\
         <region> sample=b.wav pitch_keycenter=64 lokey=63 hikey=127\n",
    );
    assert_eq!(st(&h).selected_region, Some(0));
    h.get_by_label("E4").click();
    h.run();
    assert_eq!(st(&h).selected_region, Some(1));
    assert_eq!(h.state().selected, st(&h).region_file(1));
    assert_eq!(shown(h.state()), "b.wav");
    h.get_by_label("C4").click();
    h.run();
    assert_eq!(st(&h).selected_region, Some(0));
    assert_eq!(shown(h.state()), "a.wav");
    assert_selection_valid(h.state());
}

#[test]
fn selecting_a_file_directly_keeps_the_region_consistent() {
    let (_dir, mut h) = open_text("<group> <region> sample=a.wav\n<region> sample=b.wav\n");
    let b = st(&h).region_file(1).unwrap();
    h.state_mut().select(b);
    assert_eq!(
        st(&h).selected_region,
        None,
        "region 0 plays a.wav, not b.wav"
    );
    h.state_mut().select_region(1);
    h.state_mut().select(b);
    assert_eq!(st(&h).selected_region, Some(1));
}

#[test]
fn delete_selected_region_keeps_selection_valid() {
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav lokey=0 hikey=40\n\
         <region> sample=b.wav lokey=41 hikey=80\n\
         <region> sample=a.wav lokey=81 hikey=127\n",
    );
    // The last region goes: the one before it is selected.
    h.state_mut().select_region(2);
    h.run();
    h.get_by_label("Delete region").click();
    h.run();
    assert_eq!(st(&h).inst().regions().len(), 2);
    assert_eq!(st(&h).selected_region, Some(1));
    assert_selection_valid(h.state());
    assert_eq!(shown(h.state()), "b.wav");

    // The first region goes: the next one slides into its place and is selected.
    h.state_mut().select_region(0);
    h.state_mut()
        .region_action(RegionListAction::DeleteRegion(0));
    assert_eq!(st(&h).selected_region, Some(0));
    assert_selection_valid(h.state());
    assert_eq!(shown(h.state()), "b.wav");

    // The last one goes: nothing is selected and the editor is empty.
    h.state_mut()
        .region_action(RegionListAction::DeleteRegion(0));
    assert_eq!(st(&h).inst().regions().len(), 0);
    assert_eq!(st(&h).selected_region, None);
    assert_eq!(h.state().selected, None);
    assert_selection_valid(h.state());
    h.run();

    // Each delete was one undo step.
    h.state_mut().undo();
    assert_eq!(st(&h).inst().regions().len(), 1);
    assert_selection_valid(h.state());
    h.state_mut().undo();
    h.state_mut().undo();
    assert_eq!(st(&h).inst().regions().len(), 3);
    assert_selection_valid(h.state());
    assert!(!st(&h).history.is_dirty());
}

#[test]
fn deleting_a_region_before_the_selected_one_follows_it() {
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav lokey=0 hikey=40\n\
         <region> sample=b.wav lokey=41 hikey=80\n\
         <region> sample=c.wav lokey=81 hikey=127\n",
    );
    h.state_mut().select_region(2);
    h.state_mut()
        .region_action(RegionListAction::DeleteRegion(0));
    assert_eq!(st(&h).selected_region, Some(1));
    assert_eq!(shown(h.state()), "c.wav");
    assert_selection_valid(h.state());
}

#[test]
fn undo_add_unassigned_keeps_selection_valid() {
    let (dir, mut h) = open_text(
        "<group> <region> sample=a.wav lokey=0 hikey=60\n\
         <region> sample=a.wav lokey=61 hikey=127\n",
    );
    h.state_mut().select_region(1);
    let c = dir.path().join("c.wav");
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(c));
    assert_eq!(st(&h).inst().regions().len(), 3);
    assert_eq!(
        st(&h).selected_region,
        Some(2),
        "the new region is selected"
    );
    assert_eq!(
        st(&h).inst().get(2, "sample").map(|g| g.0).as_deref(),
        Some("c.wav")
    );
    assert_eq!(
        st(&h).inst().region_info(2).keycenter,
        Keycenter::Note(72),
        "the WAV's root"
    );
    assert_selection_valid(h.state());
    assert_eq!(shown(h.state()), "c.wav");
    assert!(st(&h).unsaved(&h.state().files));

    // Undo takes the region away; the selection falls back to one that is still there.
    h.state_mut().undo();
    assert_eq!(st(&h).inst().regions().len(), 2);
    assert_eq!(st(&h).selected_region, Some(1));
    assert_selection_valid(h.state());
    assert_eq!(shown(h.state()), "a.wav");
    h.state_mut().redo();
    assert_eq!(st(&h).inst().regions().len(), 3);
    assert_selection_valid(h.state());
    h.run();
}

#[test]
fn undo_add_to_an_empty_instrument_leaves_nothing_selected() {
    let (dir, mut h) = open_text("<group>\n");
    assert_eq!(st(&h).selected_region, None);
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(dir.path().join("a.wav")));
    assert_eq!(st(&h).inst().regions().len(), 1);
    assert_eq!(st(&h).selected_region, Some(0));
    assert_selection_valid(h.state());
    h.state_mut().undo();
    assert_eq!(st(&h).inst().regions().len(), 0);
    assert_eq!(st(&h).selected_region, None);
    assert_eq!(h.state().selected, None);
    assert_selection_valid(h.state());
    h.run();
}

#[test]
fn undo_follows_the_most_recent_edit_across_histories() {
    let (dir, mut h) = open_text("<group> <region> sample=a.wav\n");
    fn sample_edit(h: &mut Harness<'static, LimewaveApp>, start: usize) {
        let i = h.state().selected.unwrap();
        let FileEntry::Loaded(s) = &mut h.state_mut().files[i] else {
            panic!("not loaded");
        };
        let e = EditState {
            start,
            ..s.edit().clone()
        };
        s.apply(e, None);
    }
    sample_edit(&mut h, 10);
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(dir.path().join("b.wav")));
    h.state_mut().select_region(0);
    sample_edit(&mut h, 20);
    // Newest first: the sample edit, then the added region, then the older sample edit.
    h.state_mut().undo();
    assert_eq!(st(&h).inst().regions().len(), 2);
    h.state_mut().undo();
    assert_eq!(st(&h).inst().regions().len(), 1);
    assert_selection_valid(h.state());
    h.state_mut().undo();
    assert_eq!(st(&h).inst().regions().len(), 1);
    let FileEntry::Loaded(s) = &h.state().files[h.state().selected.unwrap()] else {
        panic!("not loaded");
    };
    assert_eq!(s.edit().start, 0, "all three are undone");
    // Redo goes the other way round.
    h.state_mut().redo();
    h.state_mut().redo();
    assert_eq!(st(&h).inst().regions().len(), 2);
    assert_selection_valid(h.state());
}

#[test]
fn add_group_appends_empty_section() {
    let (_dir, mut h) = open_text("<group> <region> sample=a.wav\n");
    assert_eq!(st(&h).inst().groups().len(), 1);
    assert!(h.query_by_label("Group 2").is_none());
    h.get_by_label("Add group").click();
    h.run();
    assert_eq!(st(&h).inst().groups().len(), 2);
    assert_eq!(st(&h).inst().regions().len(), 1);
    h.get_by_label("Group 2");
    assert_eq!(st(&h).selected_region, Some(0), "the selection stays");
    assert!(st(&h).history.is_dirty());
    h.state_mut().undo();
    h.run();
    assert_eq!(st(&h).inst().groups().len(), 1);
}

#[test]
fn unassigned_lists_unused_wavs() {
    let (dir, mut h) = open_text("<group> <region> sample=a.wav\n");
    write_wav(dir.path(), "D.WAV", None);
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
    fn names(h: &Harness<'static, LimewaveApp>) -> Vec<String> {
        unassigned_wavs(st(h), &h.state().files)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }
    assert_eq!(names(&h), ["b.wav", "c.wav", "D.WAV"]);

    // Using one takes it off the list; deleting that region puts it back.
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(dir.path().join("c.wav")));
    assert_eq!(names(&h), ["b.wav", "D.WAV"]);
    h.state_mut()
        .region_action(RegionListAction::DeleteRegion(1));
    assert_eq!(names(&h), ["b.wav", "c.wav", "D.WAV"]);

    // The panel shows the count; opening the section lists the rows.
    h.run();
    h.get_by_label("Unassigned WAVs (3)").click();
    h.run();
    h.get_by_label("D.WAV");
    assert_eq!(h.query_all_by_label("Add").count(), 3);
}

#[test]
fn add_unassigned_names_the_wav_relative_to_the_instrument() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("Samples");
    std::fs::create_dir(&sub).unwrap();
    write_wav(&sub, "a.wav", Some(60));
    write_wav(&sub, "z.wav", Some(50));
    let path = dir.path().join("inst.sfz");
    std::fs::write(&path, "<group> <region> sample=Samples\\a.wav\n").unwrap();
    let mut h = open(&path);
    let found: Vec<PathBuf> = unassigned_wavs(st(&h), &h.state().files);
    assert_eq!(found, [sub.join("z.wav")]);
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(sub.join("z.wav")));
    assert_eq!(
        st(&h).inst().get(1, "sample").map(|g| g.0).as_deref(),
        Some("Samples/z.wav")
    );
    assert!(st(&h).region_file(1).is_some(), "the new region is bound");
    assert_eq!(st(&h).region_file(1), h.state().selected);
}

#[test]
fn rows_with_unsaved_changes_show_a_dot() {
    let (dir, mut h) = open_text(
        "<group> <region> sample=a.wav lokey=0 hikey=60\n\
         <region> sample=b.wav lokey=61 hikey=127\n",
    );
    let dots = |h: &Harness<'static, LimewaveApp>| unsaved_regions(st(h), &h.state().files);
    assert_eq!(dots(&h), [false, false]);

    // A new region is unsaved while the others stay clean; undo takes the dot away.
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(dir.path().join("c.wav")));
    assert_eq!(dots(&h), [false, false, true]);
    h.state_mut().undo();
    assert_eq!(dots(&h), [false, false]);

    // A changed opcode marks that region only, wherever it sits.
    h.state_mut()
        .instrument
        .as_mut()
        .unwrap()
        .edit(|i| i.set(1, "volume", "-3"));
    assert_eq!(dots(&h), [false, true]);
    // Deleting the region before it moves it up a number, and it is still the changed one.
    h.state_mut()
        .region_action(RegionListAction::DeleteRegion(0));
    assert_eq!(dots(&h), [true]);
    h.state_mut().undo();
    h.state_mut().undo();
    assert_eq!(dots(&h), [false, false]);
    h.run();
}

#[test]
fn saved_state_clears_the_dots_and_untitled_marks_all() {
    let (dir, mut h) = open_text(
        "<group> <region> sample=a.wav lokey=0 hikey=60\n\
         <region> sample=b.wav lokey=61 hikey=127\n",
    );
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(dir.path().join("c.wav")));
    let st_mut = h.state_mut().instrument.as_mut().unwrap();
    st_mut.history.mark_saved();
    assert_eq!(
        unsaved_regions(st(&h), &h.state().files),
        [false, false, false]
    );
    // After a push drops the saved entry, everything is unsaved.
    h.state_mut().undo();
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(dir.path().join("c.wav")));
    assert!(st(&h).history.saved().is_none());
    assert_eq!(
        unsaved_regions(st(&h), &h.state().files),
        [true, true, true]
    );
    // An untitled instrument counts everything too, saved entry or not.
    h.state_mut()
        .instrument
        .as_mut()
        .unwrap()
        .history
        .mark_saved();
    assert_eq!(
        unsaved_regions(st(&h), &h.state().files),
        [false, false, false]
    );
    h.state_mut().instrument.as_mut().unwrap().untitled = true;
    assert_eq!(
        unsaved_regions(st(&h), &h.state().files),
        [true, true, true]
    );
}

#[test]
fn unassigned_count_follows_add_and_undo() {
    let (dir, mut h) = open_text("<group> <region> sample=a.wav\n");
    h.run();
    h.get_by_label("Unassigned WAVs (2)");
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(dir.path().join("c.wav")));
    h.run();
    h.get_by_label("Unassigned WAVs (1)");
    h.state_mut().undo();
    h.run();
    h.get_by_label("Unassigned WAVs (2)");
}

/// The key map's box area on screen.
fn map_area(h: &Harness<'static, LimewaveApp>) -> Rect {
    h.get_by_label("Key map").rect()
}

fn press(h: &mut Harness<'static, LimewaveApp>, at: Pos2, button: PointerButton, pressed: bool) {
    h.event(Event::PointerButton {
        pos: at,
        button,
        pressed,
        modifiers: Modifiers::NONE,
    });
    h.step();
}

fn click_at(h: &mut Harness<'static, LimewaveApp>, at: Pos2) {
    h.hover_at(at);
    h.step();
    press(h, at, PointerButton::Primary, true);
    press(h, at, PointerButton::Primary, false);
    h.run();
}

fn drag_with(h: &mut Harness<'static, LimewaveApp>, button: PointerButton, from: Pos2, to: Pos2) {
    h.hover_at(from);
    h.step();
    press(h, from, button, true);
    for i in 1..=4 {
        h.hover_at(from + (to - from) * (i as f32 / 4.0));
        h.step();
    }
    press(h, to, button, false);
    h.run();
}

fn keys(st: &InstrumentState, region: usize) -> (u8, u8) {
    let info = st.inst().region_info(region);
    (info.lokey, info.hikey)
}

#[test]
fn clicking_a_box_selects_its_region() {
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav pitch_keycenter=60 lokey=0 hikey=62\n\
         <region> sample=b.wav pitch_keycenter=64 lokey=63 hikey=127\n",
    );
    h.run();
    assert_eq!(st(&h).selected_region, Some(0));
    let b = box_rect(map_area(&h), false, 0, 1, 63, 127, 1, 127);
    click_at(&mut h, b.center());
    assert_eq!(st(&h).selected_region, Some(1));
    assert_eq!(shown(h.state()), "b.wav");
    let a = box_rect(map_area(&h), false, 0, 1, 0, 62, 1, 127);
    click_at(&mut h, a.center());
    assert_eq!(st(&h).selected_region, Some(0));
    assert_selection_valid(h.state());
    assert!(!st(&h).history.is_dirty(), "selecting is not an edit");
}

#[test]
fn clicking_again_where_boxes_overlap_cycles_through_them() {
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav lokey=40 hikey=80\n\
         <region> sample=b.wav lokey=50 hikey=90\n",
    );
    h.run();
    let at = box_rect(map_area(&h), false, 0, 1, 60, 70, 1, 127).center();
    click_at(&mut h, at);
    assert_eq!(
        st(&h).selected_region,
        Some(0),
        "the selected one stays first"
    );
    click_at(&mut h, at);
    assert_eq!(st(&h).selected_region, Some(1));
    click_at(&mut h, at);
    assert_eq!(st(&h).selected_region, Some(0));
}

#[test]
fn dragging_a_right_edge_changes_hikey_in_one_undo_step() {
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav pitch_keycenter=60 lokey=0 hikey=62\n\
         <region> sample=b.wav pitch_keycenter=64 lokey=63 hikey=127\n",
    );
    h.run();
    let area = map_area(&h);
    let a = box_rect(area, false, 0, 1, 0, 62, 1, 127);
    let from = Pos2::new(a.right() - 1.0, a.center().y);
    let to = box_rect(area, false, 0, 1, 72, 72, 1, 127).center();
    drag_with(&mut h, PointerButton::Primary, from, to);
    assert_eq!(keys(st(&h), 0), (0, 72));
    assert_eq!(keys(st(&h), 1), (63, 127), "the other region stays");
    assert_eq!(st(&h).history.cursor(), 1, "one undo step");
    assert!(st(&h).preview.is_none(), "the preview was committed");
    h.state_mut().undo();
    assert_eq!(keys(st(&h), 0), (0, 62));
    assert!(!st(&h).history.is_dirty());
}

#[test]
fn auto_map_sorts_by_the_key_each_region_sounds_at() {
    // Region 1 is transposed two octaves down: its sample plays at its recorded pitch on
    // key 84, so it maps above region 2.
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav pitch_keycenter=48\n\
         <region> sample=b.wav pitch_keycenter=60 transpose=-24\n\
         <region> sample=c.wav pitch_keycenter=72\n",
    );
    h.state_mut().select_region(0);
    h.run();
    h.get_by_label("Auto-map from root keys").click();
    h.run();
    let s = st(&h);
    assert_eq!(keys(s, 0), (0, 60));
    assert_eq!(keys(s, 2), (61, 78));
    assert_eq!(keys(s, 1), (79, 127));
}

#[test]
fn auto_map_lays_out_ranges_by_root() {
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav pitch_keycenter=48\n\
         <region> sample=b.wav pitch_keycenter=60 lokey=60 hikey=60\n\
         <region> sample=c.wav pitch_keycenter=72 lovel=1 hivel=64\n\
         <group> <region> sample=a.wav pitch_keycenter=48 lokey=10 hikey=20\n",
    );
    h.run();
    h.get_by_label("Auto-map from root keys").click();
    h.run();
    let s = st(&h);
    assert_eq!(keys(s, 0), (0, 54));
    assert_eq!(keys(s, 1), (55, 66));
    assert_eq!(keys(s, 2), (67, 127));
    assert_eq!(keys(s, 3), (10, 20), "another group is left alone");
    assert_eq!(s.inst().region_info(2).hivel, 64, "velocity is not changed");
    assert_eq!(s.history.cursor(), 1, "one undo step");
    h.state_mut().undo();
    assert_eq!(keys(st(&h), 0), (0, 127));
    assert_eq!(keys(st(&h), 1), (60, 60));
}

#[test]
fn auto_map_uses_the_samples_current_root() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav pitch_keycenter=48\n<region> sample=b.wav pitch_keycenter=60\n",
    );
    // Region 0's sample is tuned to 64 in the editor: it is now above region 1.
    let i = h.state().selected.unwrap();
    let FileEntry::Loaded(s) = &mut h.state_mut().files[i] else {
        panic!("not loaded");
    };
    let e = EditState {
        root: 64,
        ..s.edit().clone()
    };
    s.apply(e, None);
    h.run();
    h.get_by_label("Auto-map from root keys").click();
    h.run();
    assert_eq!(keys(st(&h), 0), (63, 127));
    assert_eq!(keys(st(&h), 1), (0, 62));
}

#[test]
fn fill_gaps_closes_holes() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav lokey=40 hikey=50\n<region> sample=b.wav lokey=60 hikey=70\n",
    );
    h.run();
    h.get_by_label("Fill gaps").click();
    h.run();
    assert_eq!(keys(st(&h), 0), (0, 55));
    assert_eq!(keys(st(&h), 1), (56, 127));
    assert_eq!(st(&h).history.cursor(), 1, "one undo step");
    h.state_mut().undo();
    assert_eq!(keys(st(&h), 0), (40, 50));
}

#[test]
fn play_map_has_one_entry_per_found_region_with_tuning() {
    let (_dir, h) = open_text(
        "<region> sample=a.wav pitch_keycenter=49 tune=14 lokey=0 hikey=60 lovel=10 hivel=90\n\
         <region> sample=b.wav pitch_keycenter=49 transpose=-1 lokey=61\n\
         <region> sample=gone.wav\n\
         <region> sample=c.wav trigger=release\n",
    );
    let app = h.state();
    let map = play_map(st(&h), &app.files);
    assert_eq!(
        map.regions.len(),
        2,
        "the missing and release regions are left out"
    );
    let (a, b) = (&map.regions[0], &map.regions[1]);
    assert_eq!((a.buffer.root, a.buffer.cents), (49, -14.0));
    assert_eq!((a.lokey, a.hikey, a.lovel, a.hivel), (0, 60, 10, 90));
    assert_eq!(
        (b.buffer.root, b.buffer.cents),
        (50, 0.0),
        "transpose -1 plays a key lower"
    );
    assert_eq!((b.lokey, b.hikey), (61, 127));
    assert_ne!(a.buffer.source, b.buffer.source);
    for r in &map.regions {
        assert!(
            app.files.iter().all(|f| match f {
                FileEntry::Loaded(s) => s.source != r.buffer.source,
                _ => true,
            }),
            "region sources are not session sources"
        );
    }
    // Rebuilding keeps each region's source.
    let again = play_map(st(&h), &app.files);
    assert_eq!(again.source, map.source);
    assert_eq!(again.regions[1].buffer.source, b.buffer.source);
}

#[test]
fn own_region_uses_its_own_values() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav pitch_keycenter=60 lokey=0 hikey=60 loop_mode=one_shot\n\
         <region> sample=a.wav pitch_keycenter=72 tune=-30 loop_mode=loop_continuous \
         loop_start=100 loop_end=2000 offset=50 end=3000 volume=-6 amp_veltrack=50 \
         ampeg_attack=0.01 ampeg_release=0.5 lokey=61\n",
    );
    assert_eq!(st(&h).binding.samples[0].follows, [true, false]);
    // The shared sample is retuned in the editor: only the following region hears it.
    let i = h.state().selected.unwrap();
    let FileEntry::Loaded(s) = &mut h.state_mut().files[i] else {
        panic!("not loaded");
    };
    let e = EditState {
        root: 55,
        ..s.edit().clone()
    };
    s.apply(e, None);
    let app = h.state();
    let map = play_map(st(&h), &app.files);
    let (follows, own) = (&map.regions[0], &map.regions[1]);
    assert_eq!(follows.buffer.root, 55);
    assert_eq!(follows.playback, Playback::OneShot);
    assert_eq!((own.buffer.root, own.buffer.cents), (72, 30.0));
    assert_eq!(
        (
            own.buffer.loop_mode,
            own.buffer.loop_start,
            own.buffer.loop_end
        ),
        (LoopMode::Forward, 100, 2000)
    );
    assert_eq!((own.buffer.start, own.buffer.end), (50, 3000));
    assert!((own.gain - 10f32.powf(-6.0 / 20.0)).abs() < 1e-6);
    assert_eq!(own.veltrack, 0.5);
    assert_eq!(own.playback, Playback::Normal);
    assert_eq!(own.adsr.attack_ms, 10.0);
    assert_eq!(own.adsr.release_ms, 500.0);
    assert!(
        std::sync::Arc::ptr_eq(&follows.buffer.audio, &own.buffer.audio),
        "regions of one sample share its audio"
    );
    // The key map's keyboard shows each region's own root.
    assert_eq!(st(&h).region_root(&app.files, 0), 55);
    assert_eq!(st(&h).region_root(&app.files, 1), 72);
}

#[test]
fn region_only_edits_reuse_the_samples_audio() {
    let (_dir, mut h) = open_text("<region> sample=a.wav\n<region> sample=b.wav lokey=61\n");
    let before = play_map(st(&h), &h.state().files);
    h.state_mut()
        .instrument
        .as_mut()
        .unwrap()
        .edit(|i| i.set(0, "volume", "-6"));
    let app = h.state();
    let after = play_map(st(&h), &app.files);
    assert_ne!(before.regions[0].gain, after.regions[0].gain);
    for (b, a) in before.regions.iter().zip(&after.regions) {
        assert!(
            std::sync::Arc::ptr_eq(&b.buffer.audio, &a.buffer.audio),
            "nothing is rendered again"
        );
    }
    let FileEntry::Loaded(s) = &app.files[st(&h).region_file(0).unwrap()] else {
        panic!("not loaded");
    };
    assert!(
        std::sync::Arc::ptr_eq(&s.rendered, &after.regions[0].buffer.audio),
        "the map shares the session's audio"
    );
}

#[test]
fn an_edit_during_a_drag_preview_is_kept() {
    let (_dir, mut h) = open_text("<region> sample=a.wav lokey=0 hikey=60\n");
    let st_mut = h.state_mut().instrument.as_mut().unwrap();
    st_mut.set_preview(|i| set_keys(i, 0, 0, 72));
    st_mut.edit(|i| i.set(0, "volume", "-3"));
    assert!(st_mut.preview.is_none(), "the edit drops the stale preview");
    st_mut.commit_preview();
    let s = st(&h);
    assert_eq!(s.inst().region_info(0).volume, -3.0, "the edit is kept");
    assert_eq!(keys(s, 0), (0, 60));
    assert_eq!(s.history.cursor(), 1);
}

#[test]
fn right_drag_on_the_key_map_keyboard_sets_the_selected_range() {
    let (_dir, mut h) = open_text("<region> sample=a.wav lokey=0 hikey=127\n");
    h.run();
    let kb = h.get_by_label("Piano keyboard").rect();
    let at = |note: u8| {
        let r = key_rect_in(kb, 0..=127, note);
        Pos2::new(r.center().x, r.bottom() - 3.0)
    };
    drag_with(&mut h, PointerButton::Secondary, at(48), at(52));
    assert_eq!(keys(st(&h), 0), (48, 52));
    assert_eq!(st(&h).history.cursor(), 1, "one undo step");
}

/// Applies `edits` as the inspector sends them.
fn region_edits(h: &mut Harness<'static, LimewaveApp>, edits: &[RegionEdit]) {
    for &e in edits {
        h.state_mut().region_edit(e);
    }
    h.run();
}

/// The effective text of `name` on region `r`, with where it came from.
fn opcode(h: &Harness<'static, LimewaveApp>, r: usize, name: &str) -> Option<(String, Level)> {
    st(h).inst().get(r, name)
}

/// Gives the selected sample a forward loop.
fn loop_selected_sample(h: &mut Harness<'static, LimewaveApp>) {
    let i = h.state().selected.unwrap();
    let FileEntry::Loaded(s) = &mut h.state_mut().files[i] else {
        panic!("not loaded");
    };
    let e = EditState {
        loop_mode: LoopMode::Forward,
        loop_start: 100,
        loop_end: 2000,
        ..s.edit().clone()
    };
    s.apply(e, None);
}

#[test]
fn region_section_edits_keys_and_volume() {
    let (_dir, mut h) = open_text("<region> sample=a.wav lokey=0 hikey=127\n");
    // A drag previews, and one commit is one undo step.
    region_edits(
        &mut h,
        &[RegionEdit::Keys(40, 50), RegionEdit::Keys(48, 52)],
    );
    assert_eq!(st(&h).history.cursor(), 0, "still a preview");
    assert_eq!(st(&h).shown().region_info(0).lokey, 48);
    region_edits(&mut h, &[RegionEdit::Commit]);
    assert_eq!(keys(st(&h), 0), (48, 52));
    assert_eq!(st(&h).history.cursor(), 1);
    assert_eq!(
        opcode(&h, 0, "lokey").unwrap().0,
        "48",
        "a numeric key stays numeric"
    );
    region_edits(&mut h, &[RegionEdit::Vel(20, 90), RegionEdit::Commit]);
    assert_eq!(opcode(&h, 0, "lovel").unwrap().0, "20");
    assert_eq!(opcode(&h, 0, "hivel").unwrap().0, "90");
    region_edits(&mut h, &[RegionEdit::Volume(-6.5), RegionEdit::Commit]);
    assert_eq!(opcode(&h, 0, "volume").unwrap().0, "-6.5");
    assert_eq!(st(&h).history.cursor(), 3);
    // The same values again change nothing, so they are no undo step either.
    region_edits(
        &mut h,
        &[
            RegionEdit::Keys(48, 52),
            RegionEdit::Volume(-6.5),
            RegionEdit::Commit,
        ],
    );
    assert_eq!(st(&h).history.cursor(), 3);
    h.state_mut().undo();
    assert!(opcode(&h, 0, "volume").is_none());
}

#[test]
fn typing_a_low_key_above_the_high_key_swaps_them() {
    let (_dir, mut h) = open_text("<region> sample=a.wav lokey=40 hikey=60\n");
    h.get_by_label("Low key").focus();
    h.run();
    h.get_by_label("Low key").type_text("C5");
    h.run();
    h.key_press(Key::Enter);
    h.run();
    assert_eq!(keys(st(&h), 0), (60, 72));
    assert_eq!(st(&h).history.cursor(), 1);
    // Text that is not a note puts the old key back.
    h.get_by_label("High key").focus();
    h.run();
    h.get_by_label("High key").type_text("zz");
    h.run();
    h.key_press(Key::Enter);
    h.run();
    assert_eq!(keys(st(&h), 0), (60, 72));
    assert_eq!(st(&h).history.cursor(), 1);
}

#[test]
fn inherited_values_are_not_written_again() {
    let (_dir, mut h) = open_text("<group> volume=-6 <region> sample=a.wav\n");
    region_edits(&mut h, &[RegionEdit::Volume(-6.0), RegionEdit::Commit]);
    assert_eq!(opcode(&h, 0, "volume"), Some(("-6".into(), Level::Group)));
    assert_eq!(st(&h).history.cursor(), 0);
    region_edits(&mut h, &[RegionEdit::Volume(-3.0), RegionEdit::Commit]);
    assert_eq!(opcode(&h, 0, "volume"), Some(("-3".into(), Level::Region)));
    assert_eq!(st(&h).inst().group_get(0, "volume").as_deref(), Some("-6"));
}

#[test]
fn envelope_edit_writes_ampeg_seconds_and_percent() {
    let (_dir, mut h) = open_text("<region> sample=a.wav ampeg_release=0.01\n");
    let adsr = Adsr {
        attack_ms: 250.0,
        decay_ms: 0.0,
        sustain: 0.75,
        release_ms: 10.0,
    };
    region_edits(&mut h, &[RegionEdit::Envelope(adsr), RegionEdit::Commit]);
    let text = |name| opcode(&h, 0, name).map(|(v, _)| v);
    assert_eq!(text("ampeg_attack").as_deref(), Some("0.25"));
    assert_eq!(text("ampeg_sustain").as_deref(), Some("75"));
    assert_eq!(text("ampeg_decay"), None, "decay is what the region had");
    assert_eq!(
        text("ampeg_release").as_deref(),
        Some("0.01"),
        "release is as read"
    );
    assert_eq!(st(&h).history.cursor(), 1);
    // The inspector reads the values back as ms and %, with no audition floor.
    let info = st(&h).inst().region_info(0);
    assert_eq!(
        (info.ampeg.attack, info.ampeg.sustain, info.ampeg.release),
        (0.25, 75.0, 0.01)
    );
}

#[test]
fn playback_one_shot_writes_loop_mode() {
    let (_dir, mut h) = open_text("<region> sample=a.wav\n");
    region_edits(&mut h, &[RegionEdit::Playback(Playback::OneShot)]);
    assert_eq!(opcode(&h, 0, "loop_mode").unwrap().0, "one_shot");
    assert_eq!(st(&h).history.cursor(), 1, "a click is its own step");
    region_edits(&mut h, &[RegionEdit::Playback(Playback::Sustain)]);
    assert_eq!(opcode(&h, 0, "loop_mode").unwrap().0, "loop_sustain");
    // Normal says no_loop while the sample does not loop, loop_continuous once it does.
    region_edits(&mut h, &[RegionEdit::Playback(Playback::Normal)]);
    assert_eq!(opcode(&h, 0, "loop_mode").unwrap().0, "no_loop");
    loop_selected_sample(&mut h);
    region_edits(&mut h, &[RegionEdit::Playback(Playback::OneShot)]);
    region_edits(&mut h, &[RegionEdit::Playback(Playback::Normal)]);
    assert_eq!(opcode(&h, 0, "loop_mode").unwrap().0, "loop_continuous");
    let steps = st(&h).history.cursor();
    region_edits(&mut h, &[RegionEdit::Playback(Playback::Normal)]);
    assert_eq!(
        st(&h).history.cursor(),
        steps,
        "already so: nothing written"
    );
}

#[test]
fn group_edit_moves_the_region_and_keeps_the_selection() {
    let (_dir, mut h) = open_text(
        "<group> <region> sample=a.wav lokey=0 hikey=40\n\
         <region> sample=b.wav lokey=41 hikey=80\n\
         <group> <region> sample=c.wav lokey=81 hikey=127\n",
    );
    h.state_mut().select_region(0);
    region_edits(&mut h, &[RegionEdit::Group(1)]);
    let inst = st(&h).inst();
    assert_eq!(inst.group_of(0), Some(0));
    assert_eq!(inst.region_info(0).sample.as_deref(), Some("b.wav"));
    assert_eq!(inst.region_info(2).sample.as_deref(), Some("a.wav"));
    assert_eq!(inst.group_of(2), Some(1), "moved to the end of group 2");
    assert_eq!(st(&h).selected_region, Some(2), "the selection follows it");
    assert_eq!(shown(h.state()), "a.wav");
    assert_selection_valid(h.state());
    h.state_mut().undo();
    assert_eq!(
        st(&h).inst().region_info(0).sample.as_deref(),
        Some("a.wav")
    );
    assert_selection_valid(h.state());
}

#[test]
fn shared_sample_says_shared_by_2() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav lokey=0 hikey=60\n<region> sample=a.wav lokey=61\n\
         <region> sample=b.wav lokey=40\n",
    );
    assert!(h.query_all_by_label("Shared by 2 regions").count() >= 1);
    h.state_mut().select_region(2);
    h.run();
    assert!(h.query_by_label_contains("Shared by").is_none());
    assert!(
        h.query_by_label("This region keeps its own tuning and loop")
            .is_none()
    );
}

#[test]
fn own_region_tuning_is_read_only() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav pitch_keycenter=60 lokey=0 hikey=60\n\
         <region> sample=a.wav pitch_keycenter=72 tune=-30 lokey=61 hikey=127\n",
    );
    assert_eq!(st(&h).binding.samples[0].follows, [true, false]);
    let disabled = |h: &Harness<'static, LimewaveApp>| {
        h.get_all_by_label("Fine tune")
            .map(|n| n.accesskit_node().is_disabled())
            .collect::<Vec<_>>()
    };
    assert_eq!(disabled(&h), [false, false], "the first region follows");
    assert!(
        h.query_by_label("This region keeps its own tuning and loop")
            .is_none()
    );
    h.state_mut().select_region(1);
    h.run();
    assert!(
        h.query_all_by_label("This region keeps its own tuning and loop")
            .count()
            >= 1
    );
    assert_eq!(disabled(&h), [true, true]);
    let root = h.get_by_label("Root key");
    assert!(root.accesskit_node().is_disabled());
    assert_eq!(
        root.accesskit_node().value().as_deref(),
        Some("C5"),
        "its own root"
    );
    assert!(h.query_by_label_contains("Shared by").is_none());

    // A right-click on a key sets the root of a region that follows, and does nothing here.
    let kb = h.get_by_label("Piano keyboard").rect();
    let key = key_rect_in(kb, 0..=127, 70);
    let at = Pos2::new(key.center().x, key.bottom() - 3.0);
    let root_of = |h: &Harness<'static, LimewaveApp>| {
        let FileEntry::Loaded(s) = &h.state().files[0] else {
            panic!("not loaded");
        };
        s.edit().root
    };
    h.hover_at(at);
    h.step();
    press(&mut h, at, PointerButton::Secondary, true);
    press(&mut h, at, PointerButton::Secondary, false);
    h.run();
    assert_eq!(root_of(&h), 60);
    assert_eq!(
        h.state().ui.message.as_deref(),
        Some("This region keeps its own tuning and loop")
    );
    h.state_mut().select_region(0);
    h.run();
    press(&mut h, at, PointerButton::Secondary, true);
    press(&mut h, at, PointerButton::Secondary, false);
    h.run();
    assert_eq!(root_of(&h), 70);
}

#[test]
fn undo_takes_back_the_newest_edit_across_instrument_and_sample() {
    let (_dir, mut h) = open_text("<region> sample=a.wav\n");
    region_edits(&mut h, &[RegionEdit::Volume(-6.0), RegionEdit::Commit]);
    loop_selected_sample(&mut h);
    let looped = |h: &Harness<'static, LimewaveApp>| {
        let FileEntry::Loaded(s) = &h.state().files[0] else {
            panic!("not loaded");
        };
        s.edit().loop_mode
    };
    assert_eq!(looped(&h), LoopMode::Forward);
    h.state_mut().undo();
    assert_eq!(looped(&h), LoopMode::Off, "the loop edit went first");
    assert!(opcode(&h, 0, "volume").is_some());
    h.state_mut().undo();
    assert!(opcode(&h, 0, "volume").is_none(), "then the volume");
    h.state_mut().redo();
    assert!(opcode(&h, 0, "volume").is_some());
    assert_eq!(looped(&h), LoopMode::Off, "redo goes the other way");
    h.state_mut().redo();
    assert_eq!(looped(&h), LoopMode::Forward);
    assert_selection_valid(h.state());
}

#[test]
fn session_envelope_hidden_with_instrument() {
    let (dir, mut h) = open_text("<region> sample=a.wav\n");
    assert!(h.query_by_label("Release plays after loop end").is_none());
    assert_eq!(h.get_all_by_label("Attack").count(), 1, "the region's own");
    h.state_mut().close_instrument();
    h.run();
    // Closing an instrument opened by itself leaves nothing open.
    h.state_mut().open_file(dir.path().join("a.wav"));
    wait_loaded(&mut h);
    assert!(h.state().instrument.is_none());
    assert!(h.query_by_label("Release plays after loop end").is_some());
    assert_eq!(h.get_all_by_label("Attack").count(), 1);
}

#[test]
fn other_settings_are_listed_and_marked() {
    let (_dir, mut h) =
        open_text("<group> polyphony=4 <region> sample=a.wav cutoff=800 pan=20 trigger=first\n");
    h.get_by_label("Other settings (kept as written)").click();
    h.run();
    h.get_by_label("cutoff");
    h.get_by_label("polyphony");
    h.get_by_label("trigger");
    assert_eq!(
        h.get_all_by_label("region, not heard in Limewave").count(),
        3
    );
    h.get_by_label("group");
}

#[test]
fn dragging_the_volume_field_is_one_undo_step() {
    let (_dir, mut h) = open_text("<region> sample=a.wav\n");
    let rect = h.get_by_label("Volume").rect();
    drag_with(
        &mut h,
        PointerButton::Primary,
        rect.center(),
        rect.center() + eframe::egui::vec2(-60.0, 0.0),
    );
    let volume = st(&h).inst().region_info(0).volume;
    assert!(volume < -1.0, "dragging left lowers it: {volume}");
    assert_eq!(st(&h).history.cursor(), 1, "one undo step");
    assert!(st(&h).preview.is_none());
    h.state_mut().undo();
    assert!(opcode(&h, 0, "volume").is_none());
}

#[test]
fn a_region_without_a_sample_still_has_its_settings() {
    let (_dir, mut h) = open_text("<region> sample=gone.wav lokey=30 hikey=40\n");
    h.state_mut().select_region(0);
    h.run();
    assert_eq!(h.state().selected, None);
    h.get_by_label("Low key");
    region_edits(&mut h, &[RegionEdit::Volume(-3.0), RegionEdit::Commit]);
    assert_eq!(opcode(&h, 0, "volume").unwrap().0, "-3");
    assert!(h.query_by_label("Fine tune").is_none());
}

#[test]
fn showing_a_region_with_out_of_range_values_writes_nothing() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav volume=12 ampeg_release=20 ampeg_attack=15 lovel=0\n\
         <region> sample=b.wav volume=-200\n",
    );
    for region in [0, 1, 0] {
        h.state_mut().select_region(region);
        h.run();
    }
    let s = st(&h);
    assert_eq!(s.history.cursor(), 0, "no undo step");
    assert!(!s.unsaved(&h.state().files));
    assert_eq!(h.state().dirty_count(), 0);
    let info = s.inst().region_info(0);
    assert_eq!(
        (info.volume, info.ampeg.release, info.lovel),
        (12.0, 20.0, 0)
    );
    assert_eq!(s.inst().region_info(1).volume, -200.0);
}

#[test]
fn numeric_key_text_stays_numeric_and_names_stay_names() {
    let (_dir, mut h) =
        open_text("<region> sample=a.wav lokey=36 hikey=c4\n<region> sample=b.wav\n");
    region_edits(&mut h, &[RegionEdit::Keys(40, 72), RegionEdit::Commit]);
    assert_eq!(
        opcode(&h, 0, "lokey").unwrap().0,
        "40",
        "numeric stays numeric"
    );
    assert_eq!(
        opcode(&h, 0, "hikey").unwrap().0,
        "C5",
        "a name stays a name"
    );
    h.state_mut().select_region(1);
    region_edits(&mut h, &[RegionEdit::Keys(40, 72), RegionEdit::Commit]);
    assert_eq!(
        opcode(&h, 1, "lokey").unwrap().0,
        "E2",
        "a new opcode is a name"
    );
    // The key map's own drag keeps the style too.
    h.state_mut().select_region(0);
    st_mut(&mut h).edit(|i| set_keys(i, 0, 41, 72));
    assert_eq!(opcode(&h, 0, "lokey").unwrap().0, "41");
}

fn st_mut<'a>(h: &'a mut Harness<'static, LimewaveApp>) -> &'a mut InstrumentState {
    h.state_mut().instrument.as_mut().unwrap()
}

/// The narrowest supported window in instrument mode: the key-map toolbar wraps, its buttons
/// work and the Region section is reachable.
#[test]
fn the_narrowest_window_keeps_the_key_map_toolbar_and_region_section_reachable() {
    let (_dir, path) = folder(
        "<region> sample=a.wav lokey=40 hikey=50\n<region> sample=b.wav lokey=60 hikey=70\n",
    );
    let mut app = LimewaveApp::for_test();
    app.open_instrument(path);
    let size = eframe::egui::vec2(900.0, 700.0);
    let mut h = Harness::builder()
        .with_size(size)
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    wait_loaded(&mut h);
    let window = Rect::from_min_size(Pos2::ZERO, size);
    let map = map_area(&h);
    for label in ["Auto-map from root keys", "Fill gaps"] {
        let r = h.get_by_label(label).rect();
        assert!(window.contains_rect(r), "{label} {r:?} inside the window");
        assert!(
            r.right() <= map.right() + 1.0,
            "{label} {r:?} inside the key map"
        );
    }
    for label in ["Low key", "Volume"] {
        let r = h.get_by_label(label).rect();
        assert!(window.contains_rect(r), "{label} {r:?}");
    }
    h.get_by_label("Fill gaps").click();
    h.run();
    assert_eq!(keys(st(&h), 0), (0, 55));
    assert_eq!(keys(st(&h), 1), (56, 127));
}

/// A folder of WAVs `a.wav`, `b.wav`, ... with the given roots, opened as a folder.
fn open_wav_folder(roots: &[u32]) -> (tempfile::TempDir, Harness<'static, LimewaveApp>) {
    let dir = tempfile::tempdir().unwrap();
    for (i, &root) in roots.iter().enumerate() {
        write_wav(
            dir.path(),
            &format!("{}.wav", (b'a' + i as u8) as char),
            Some(root),
        );
    }
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    (dir, h)
}

#[test]
fn new_instrument_maps_open_wavs_by_root() {
    let (dir, mut h) = open_wav_folder(&[48, 60, 72]);
    h.get_by_label("New instrument").click();
    h.run();
    let app = h.state();
    let s = app.instrument.as_ref().expect("the instrument opened");
    assert_eq!(s.inst().regions().len(), 3);
    assert_eq!(keys(s, 0), (0, 54));
    assert_eq!(keys(s, 1), (55, 66));
    assert_eq!(keys(s, 2), (67, 127));
    for (r, (name, root)) in [("a.wav", 48), ("b.wav", 60), ("c.wav", 72)]
        .into_iter()
        .enumerate()
    {
        assert_eq!(opcode(&h, r, "sample").unwrap().0, name);
        assert_eq!(
            opcode(&h, r, "pitch_keycenter").unwrap().0,
            root.to_string()
        );
        assert_eq!(s.region_file(r), Some(r));
    }
    assert_eq!(s.path, dir.path().join("Untitled instrument.sfz"));
    assert_eq!(s.inst().file.crlf, cfg!(windows));
    assert_eq!(s.selected_region, Some(0));
    assert_eq!(app.selected, Some(0));
    assert!(matches!(
        &s.return_to,
        Some(PendingAction::OpenFolder(d)) if d == dir.path()
    ));
    assert_eq!(play_map(s, &app.files).regions.len(), 3);
    assert!(
        s.binding
            .samples
            .iter()
            .all(|b| b.follows.iter().all(|&f| f))
    );
}

#[test]
fn new_instrument_is_unsaved_and_wavs_untouched() {
    let (_dir, mut h) = open_wav_folder(&[48, 60]);
    // A sample whose root was changed in the editor keeps that root, and stays its own edit.
    let FileEntry::Loaded(s) = &mut h.state_mut().files[1] else {
        panic!("not loaded");
    };
    let e = EditState {
        root: 50,
        ..s.edit().clone()
    };
    s.apply(e, None);
    assert_eq!(h.state().dirty_count(), 1);
    h.state_mut().new_instrument();
    h.run();
    let app = h.state();
    assert_eq!(app.dirty_count(), 2, "the instrument, and the changed WAV");
    for (i, f) in app.files.iter().enumerate() {
        let FileEntry::Loaded(s) = f else {
            panic!("not loaded");
        };
        assert_eq!(s.unsaved(), i == 1, "only the WAV edited before is unsaved");
    }
    let s = app.instrument.as_ref().unwrap();
    assert!(s.untitled && s.unsaved(&app.files));
    // The region says the WAV's own root and follows the sample, which sounds at 50: mapped
    // there now, and written by a save with the other edits made before.
    assert_eq!(opcode(&h, 1, "pitch_keycenter").unwrap().0, "60");
    assert_eq!(s.binding.samples[1].follows, [true]);
    assert_eq!(s.region_root(&app.files, 1), 50);
    assert_eq!(keys(s, 0), (0, 49));
    assert_eq!(keys(s, 1), (50, 127));
    // Leaving an untitled instrument returns to the folder.
    h.state_mut().close_instrument();
    wait_loaded(&mut h);
    assert!(h.state().instrument.is_none());
    assert_eq!(h.state().files.len(), 2);
}

#[test]
fn new_instrument_uses_only_loaded_wavs() {
    // One entry fails to load, the other is fine: only the loaded WAV gets a region.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.wav"), b"not a wav").unwrap();
    write_wav(dir.path(), "b.wav", Some(60));
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    assert!(matches!(h.state().files[0], FileEntry::Failed(..)));
    h.state_mut().new_instrument();
    h.run();
    let s = st(&h);
    assert_eq!(s.inst().regions().len(), 1);
    assert_eq!(opcode(&h, 0, "sample").unwrap().0, "b.wav");
    assert_eq!(keys(s, 0), (0, 127));
    assert_eq!(s.region_file(0), Some(1));
}

#[test]
fn button_disabled_without_wavs() {
    // An empty folder.
    let (_dir, mut h) = open_wav_folder(&[]);
    assert!(
        h.get_by_label("New instrument")
            .accesskit_node()
            .is_disabled()
    );
    h.state_mut().new_instrument();
    assert!(h.state().instrument.is_none());
    // No folder at all, and a single file: the button is not offered, and does nothing.
    let mut h = harness(LimewaveApp::for_test());
    assert!(h.query_by_label("New instrument").is_none());
    h.state_mut().new_instrument();
    assert!(h.state().instrument.is_none());
    let dir = tempfile::tempdir().unwrap();
    let wav = write_wav(dir.path(), "a.wav", Some(60));
    let mut app = LimewaveApp::for_test();
    app.open_file(wav);
    let mut h = harness(app);
    assert!(h.query_by_label("New instrument").is_none());
    h.state_mut().new_instrument();
    assert!(h.state().instrument.is_none());
    // Enabled with WAVs, gone once an instrument is open.
    let (_dir, mut h) = open_wav_folder(&[60]);
    assert!(
        !h.get_by_label("New instrument")
            .accesskit_node()
            .is_disabled()
    );
    h.state_mut().new_instrument();
    h.run();
    // The region list takes the button's place.
    assert!(h.query_by_label("New instrument").is_none());
    let before = h.state().instrument.as_ref().unwrap().history.stamp();
    h.state_mut().new_instrument();
    assert_eq!(
        h.state().instrument.as_ref().unwrap().history.stamp(),
        before
    );
}

#[test]
fn new_instrument_keeps_file_names_with_spaces_and_leaves_out_unwritable_ones() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(dir.path(), "EP A1 mp.wav", Some(33));
    write_wav(dir.path(), "EP C2 mp.wav", Some(36));
    // `=` would end the name early; it can't be written.
    write_wav(dir.path(), "x y=z.wav", Some(40));
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    h.state_mut().new_instrument();
    h.run();
    assert_eq!(st(&h).inst().regions().len(), 2);
    assert_eq!(opcode(&h, 0, "sample").unwrap().0, "EP A1 mp.wav");
    assert_eq!(opcode(&h, 1, "sample").unwrap().0, "EP C2 mp.wav");
    assert_eq!(opcode(&h, 1, "pitch_keycenter").unwrap().0, "36");
    assert_eq!(st(&h).region_file(1), Some(1));
    assert_eq!(
        h.state().ui.message.as_deref(),
        Some("1 file was left out: its name can't be written in an SFZ file")
    );
}

#[test]
fn new_instrument_with_only_unwritable_names_makes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(dir.path(), "$x.wav", Some(40));
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    let mut h = harness(app);
    wait_loaded(&mut h);
    h.state_mut().new_instrument();
    assert!(h.state().instrument.is_none());
    assert!(h.state().ui.message.is_some());
}

#[test]
fn add_unassigned_refuses_a_name_that_cannot_be_written() {
    let (dir, mut h) = open_text("<region> sample=a.wav\n");
    let bad = write_wav(dir.path(), "x y=z.wav", Some(50));
    let files = h.state().files.len();
    h.state_mut()
        .region_action(RegionListAction::AddUnassigned(bad));
    h.run();
    assert_eq!(st(&h).inst().regions().len(), 1);
    assert!(!st(&h).history.is_dirty());
    assert_eq!(h.state().files.len(), files, "the WAV is not opened either");
    assert_eq!(
        h.state().ui.message.as_deref(),
        Some("x y=z.wav can't be added: its name can't be written in an SFZ file")
    );
}

#[test]
fn deleting_the_first_region_keeps_the_others_own_values() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav pitch_keycenter=60 hikey=59\n\
         <region> sample=a.wav pitch_keycenter=62 lokey=60\n",
    );
    assert_eq!(st(&h).binding.samples[0].follows, [true, false]);
    h.state_mut()
        .region_action(RegionListAction::DeleteRegion(0));
    let s = st(&h);
    assert_eq!(s.inst().regions().len(), 1);
    assert_eq!(
        s.binding.samples[0].follows,
        [false],
        "the sample's state stays the deleted region's"
    );
    assert_eq!(s.region_root(&h.state().files, 0), 62);
}

#[test]
fn a_one_shot_region_leaves_its_sibling_following() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav loop_mode=loop_continuous loop_start=100 loop_end=900 hikey=59\n\
         <region> sample=a.wav loop_mode=loop_continuous loop_start=100 loop_end=900 lokey=60\n",
    );
    assert_eq!(st(&h).binding.samples[0].follows, [true, true]);
    for region in [0, 1] {
        h.state_mut().select_region(region);
        region_edits(&mut h, &[RegionEdit::Playback(Playback::OneShot)]);
        assert_eq!(
            st(&h).binding.samples[0].follows,
            [true, true],
            "region {region} one-shot"
        );
        h.state_mut().undo();
    }
    // With region 1 one-shot, a loop edit on the sample is heard on both.
    h.state_mut().select_region(1);
    region_edits(&mut h, &[RegionEdit::Playback(Playback::OneShot)]);
    let i = h.state().selected.unwrap();
    let FileEntry::Loaded(s) = &mut h.state_mut().files[i] else {
        panic!("not loaded");
    };
    let e = EditState {
        loop_start: 200,
        loop_end: 800,
        ..s.edit().clone()
    };
    s.apply(e, None);
    let map = play_map(st(&h), &h.state().files);
    for r in &map.regions {
        assert_eq!((r.buffer.loop_start, r.buffer.loop_end), (200, 800));
    }
    assert_eq!(map.regions[1].playback, Playback::OneShot);
}

#[test]
fn a_no_loop_region_following_a_looped_sample_plays_without_the_loop() {
    let (_dir, mut h) = open_text(
        "<region> sample=a.wav loop_mode=loop_continuous loop_start=100 loop_end=900 hikey=59\n\
         <region> sample=a.wav loop_mode=loop_continuous loop_start=100 loop_end=900 lokey=60\n",
    );
    st_mut(&mut h).edit(|i| i.set(1, "loop_mode", "no_loop"));
    assert_eq!(st(&h).binding.samples[0].follows, [true, true]);
    let map = play_map(st(&h), &h.state().files);
    assert_eq!(map.regions[0].buffer.loop_mode, LoopMode::Forward);
    assert_eq!(map.regions[1].buffer.loop_mode, LoopMode::Off);
}

#[test]
fn region_settings_are_read_once_per_change() {
    let (_dir, mut h) =
        open_text("<region> sample=a.wav lokey=0 hikey=59\n<region> sample=b.wav\n");
    let first = st(&h).view();
    assert!(
        std::sync::Arc::ptr_eq(&first, &st(&h).view()),
        "kept while nothing changes"
    );
    assert_eq!(first.infos.len(), 2);
    h.run();
    assert!(
        std::sync::Arc::ptr_eq(&first, &st(&h).view()),
        "kept across frames"
    );
    // A drag preview, and dropping it, are seen at once.
    st_mut(&mut h).set_preview(|i| set_keys(i, 0, 0, 40));
    assert_eq!(st(&h).view().infos[0].hikey, 40);
    st_mut(&mut h).drop_preview();
    assert_eq!(st(&h).view().infos[0].hikey, 59);
    st_mut(&mut h).edit(|i| set_keys(i, 0, 0, 50));
    assert_eq!(st(&h).view().infos[0].hikey, 50);
    h.state_mut().undo();
    assert_eq!(st(&h).view().infos[0].hikey, 59);
    assert_eq!(st(&h).view().groups, [None, None]);
    // The unsaved rows follow the history the same way.
    assert_eq!(unsaved_regions(st(&h), &h.state().files), [false, false]);
    h.state_mut().redo();
    assert_eq!(unsaved_regions(st(&h), &h.state().files), [true, false]);
}
