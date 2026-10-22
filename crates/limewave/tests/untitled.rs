//! Untitled documents: in memory only, unsaved from the moment they exist, saved through a
//! file picker.

use eframe::egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, NewName, PendingAction, Session};
use limewave::ui::new_dialog::{BitDepth, NewDialogState};
use limewave_core::document::new_document;
use limewave_core::edit::{EditState, LoopMode};
use limewave_core::save::Destination;
use limewave_core::wav::SampleFormat;

/// Adds a silent 16-bit mono untitled document and returns its index.
fn add(app: &mut LimewaveApp) -> usize {
    let name = app.next_name(NewName::Untitled);
    app.add_untitled(new_document(
        name,
        vec![vec![0.0; 1000]],
        44_100,
        SampleFormat::Int { bits: 16 },
        60,
        false,
    ));
    app.files.len() - 1
}

fn session(app: &mut LimewaveApp, i: usize) -> &mut Session {
    match &mut app.files[i] {
        FileEntry::Loaded(s) => s,
        _ => panic!("file is not loaded"),
    }
}

fn harness(app: LimewaveApp) -> Harness<'static, LimewaveApp> {
    let mut h = Harness::builder()
        .with_size(vec2(1280.0, 900.0))
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    h.run();
    h
}

#[test]
fn untitled_files_count_as_unsaved_from_creation() {
    let mut app = LimewaveApp::for_test();
    add(&mut app);
    assert_eq!(app.dirty_count(), 1);
    assert_eq!(app.files[0].file_name(), "Untitled 1.wav");
    assert_eq!(app.selected, Some(0));

    add(&mut app);
    assert_eq!(app.files[1].file_name(), "Untitled 2.wav");
    assert_eq!(app.selected, Some(1));
    assert_eq!(app.dirty_count(), 2);

    let mut h = harness(app);
    h.run();
    assert_eq!(h.state().dirty_count(), 2);
}

#[test]
fn recordings_have_their_own_counter() {
    let mut app = LimewaveApp::for_test();
    assert_eq!(
        app.next_name(NewName::Recording).to_string_lossy(),
        "Recording 1.wav"
    );
    assert_eq!(
        app.next_name(NewName::Untitled).to_string_lossy(),
        "Untitled 1.wav"
    );
    assert_eq!(
        app.next_name(NewName::Recording).to_string_lossy(),
        "Recording 2.wav"
    );
}

#[test]
fn opening_a_folder_with_an_untitled_file_asks_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = LimewaveApp::for_test();
    add(&mut app);
    app.request(PendingAction::OpenFolder(dir.path().to_path_buf()));
    assert!(app.pending.is_some());
    assert_eq!(app.files.len(), 1);
}

#[test]
fn save_all_with_a_cancelled_picker_keeps_the_held_action_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = LimewaveApp::for_test();
    add(&mut app);
    app.request(PendingAction::OpenFolder(dir.path().to_path_buf()));
    app.resolve_pending_save_all();
    assert_eq!(app.files.len(), 1, "the held action did not run");
    assert!(session(&mut app, 0).untitled);
    assert_eq!(app.dirty_count(), 1);
}

#[test]
fn saving_an_untitled_file_makes_it_an_ordinary_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("mine.wav");
    let mut app = LimewaveApp::for_test();
    add(&mut app);
    let picked = target.clone();
    app.save_picker = Box::new(move |name| {
        assert_eq!(name, "Untitled 1.wav");
        Some(picked.clone())
    });
    app.save();

    let s = session(&mut app, 0);
    assert!(!s.untitled);
    assert!(!s.unsaved());
    assert!(s.doc.path.ends_with("mine.wav"));
    assert!(target.exists());
    assert_eq!(app.dirty_count(), 0);
    assert!(app.save_results.is_none());

    // An edit and a second save replace the same file without leaving a backup.
    let s = session(&mut app, 0);
    let e = EditState {
        loop_mode: LoopMode::Forward,
        loop_start: 100,
        loop_end: 800,
        ..s.edit().clone()
    };
    s.apply(e, None);
    assert_eq!(app.dirty_count(), 1);
    app.save_picker = Box::new(|_| panic!("the second save does not ask"));
    app.save();
    assert_eq!(app.dirty_count(), 0);
    assert!(app.save_results.is_none());
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["mine.wav".to_owned()], "no backup file");
}

#[test]
fn a_picked_name_without_an_extension_gets_wav() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = LimewaveApp::for_test();
    add(&mut app);
    let picked = dir.path().join("mine");
    app.save_picker = Box::new(move |_| Some(picked.clone()));
    app.save();
    let s = session(&mut app, 0);
    assert!(!s.untitled);
    assert!(s.doc.path.ends_with("mine.wav"));
    assert!(dir.path().join("mine.wav").exists());
    assert!(!dir.path().join("mine").exists());
}

#[test]
fn a_picked_name_with_a_dot_but_no_wav_gets_wav_added() {
    for (picked, saved) in [
        ("kick.01", "kick.01.wav"),
        ("pad v1.2", "pad v1.2.wav"),
        ("Loud.WAV", "Loud.WAV"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = LimewaveApp::for_test();
        add(&mut app);
        let path = dir.path().join(picked);
        app.save_picker = Box::new(move |_| Some(path.clone()));
        app.save();
        let s = session(&mut app, 0);
        assert!(
            s.doc.path.ends_with(saved),
            "{picked} saved as {:?}",
            s.doc.path
        );
        assert!(dir.path().join(saved).exists(), "{picked}");
    }
}

#[test]
fn a_cancelled_picker_leaves_the_file_untitled() {
    let mut app = LimewaveApp::for_test();
    add(&mut app);
    app.save();
    assert!(session(&mut app, 0).untitled);
    assert_eq!(app.dirty_count(), 1);
    assert!(app.save_results.is_none());
}

#[test]
fn the_batch_save_dialog_leaves_untitled_files_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone.wav");
    let bytes = limewave_core::testwav::wav_bytes(
        &[limewave_core::testwav::sine(440.0, 44100, 2000, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    std::fs::write(&path, bytes).unwrap();

    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    let untitled = add(&mut app);
    let s = session(&mut app, 0);
    let e = EditState {
        loop_mode: LoopMode::Forward,
        loop_start: 100,
        loop_end: 800,
        ..s.edit().clone()
    };
    s.apply(e, None);

    app.ui.save_dialog.open_for(0, &app.files, true);
    app.ui.save_dialog.scope_all = true;
    app.ui.save_dialog.only_changed = false;
    let chosen = app.ui.save_dialog.chosen(&app.files);
    assert_eq!(chosen, vec![0]);
    assert!(!chosen.contains(&untitled));
}

#[test]
fn save_all_stopped_by_a_cancelled_picker_still_reports_earlier_failures() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone.wav");
    let bytes = limewave_core::testwav::wav_bytes(
        &[limewave_core::testwav::sine(440.0, 44100, 2000, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    std::fs::write(&path, &bytes).unwrap();
    // A different file with the same name where the first file remembers saving to: a
    // save without a dialog refuses to replace it.
    std::fs::write(elsewhere.path().join("tone.wav"), b"not the same file").unwrap();

    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    let s = session(&mut app, 0);
    s.destination = Some(Destination::Folder(elsewhere.path().to_path_buf()));
    let e = EditState {
        loop_mode: LoopMode::Forward,
        loop_start: 100,
        loop_end: 800,
        ..s.edit().clone()
    };
    s.apply(e, None);
    add(&mut app);

    // The test picker cancels.
    assert!(!app.save_all());
    let results = app.save_results.as_ref().expect("the failure is shown");
    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_err());
    assert!(session(&mut app, 1).untitled);
}

#[test]
fn create_new_adds_a_silent_selected_file_and_turns_draw_on() {
    let mut app = LimewaveApp::for_test();
    app.create_new(441, 48_000, 2, BitDepth::Int24, 62, true);
    assert_eq!(app.files.len(), 1);
    assert_eq!(app.selected, Some(0));
    assert!(app.ui.draw);
    let s = session(&mut app, 0);
    assert!(s.untitled);
    assert_eq!(s.doc.format.channels, 2);
    assert_eq!(s.doc.format.sample_rate, 48_000);
    assert_eq!(s.doc.format.format, SampleFormat::Int { bits: 24 });
    assert_eq!(s.doc.frames, 441);
    assert!(s.doc.audio.iter().all(|c| c.len() == 441));
    assert!(s.doc.audio.iter().flatten().all(|v| *v == 0.0));
    let e = s.edit();
    assert_eq!(e.root, 62);
    assert_eq!(e.loop_mode, LoopMode::Forward);
    assert_eq!((e.loop_start, e.loop_end), (0, 440));
}

#[test]
fn the_new_button_opens_the_dialog_and_create_uses_the_defaults() {
    let mut h = harness(LimewaveApp::for_test());
    h.get_by_label("New…").click();
    h.run();
    assert!(h.state().ui.new_dialog.is_some());
    h.get_by_label("Create").click();
    h.run();
    let app = h.state_mut();
    assert!(app.ui.new_dialog.is_none());
    assert_eq!(app.files.len(), 1);
    assert_eq!(app.files[0].file_name(), "Untitled 1.wav");
    let s = session(app, 0);
    assert_eq!(s.doc.frames, 44_100);
    assert_eq!(s.doc.format.channels, 1);
    assert_eq!(s.doc.format.sample_rate, 44_100);
    assert_eq!(s.doc.format.format, SampleFormat::Int { bits: 16 });
}

#[test]
fn cancel_closes_the_new_dialog_without_a_file() {
    let mut h = harness(LimewaveApp::for_test());
    h.get_by_label("New…").click();
    h.run();
    h.get_by_label("Cancel").click();
    h.run();
    assert!(h.state().ui.new_dialog.is_none());
    assert!(h.state().files.is_empty());
}

#[test]
fn an_over_limit_length_shows_the_limit_and_disables_create() {
    let mut app = LimewaveApp::for_test();
    app.ui.new_dialog = Some(NewDialogState {
        length: 600_001.0,
        ..NewDialogState::default()
    });
    let mut h = harness(app);
    assert!(h.get_by_label("Create").accesskit_node().is_disabled());
    h.get_by_label("1 frame to 600 s");

    h.state_mut().ui.new_dialog.as_mut().unwrap().length = 1000.0;
    h.run();
    assert!(!h.get_by_label("Create").accesskit_node().is_disabled());
    assert!(h.query_by_label("1 frame to 600 s").is_none());
}
