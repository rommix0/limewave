//! Saving driven through the whole app: shortcuts, the dialog, the results window and the
//! unsaved-changes prompt.

use std::path::{Path, PathBuf};

use eframe::egui::{Key, Modifiers, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use limewave::app::{FileEntry, LimewaveApp, PendingAction, Session};
use limewave::ui::spectrogram::{block_until_ready, picture_for};
use limewave_core::document::load_wav;
use limewave_core::edit::{EditState, LoopMode};
use limewave_core::save::Destination;
use limewave_core::software_tag;
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

/// A 16-bit mono 440 Hz sine of 8000 frames at `dir/name`, no metadata.
fn write_wav(dir: &Path, name: &str) -> PathBuf {
    let bytes = wav_bytes(
        &[sine(440.0, 44100, 8000, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn session(app: &mut LimewaveApp) -> &mut Session {
    match &mut app.files[0] {
        FileEntry::Loaded(s) => s,
        _ => panic!("file did not load"),
    }
}

/// `path` opened on its own, looped 1000..=5000 with a 50-frame crossfade (unsaved).
fn edited_app(path: PathBuf) -> LimewaveApp {
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    let s = session(&mut app);
    let e = EditState {
        loop_mode: LoopMode::Forward,
        loop_start: 1000,
        loop_end: 5000,
        crossfade: 50,
        ..s.edit().clone()
    };
    s.apply(e, None);
    app
}

fn harness(app: LimewaveApp) -> Harness<'static, LimewaveApp> {
    let mut h = Harness::builder()
        .with_size(vec2(1280.0, 900.0))
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    h.run();
    h
}

#[test]
fn a_backward_loop_is_not_reported_as_crossfaded() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_wav(dir.path(), "tone.wav");
    let mut app = edited_app(source);
    let s = session(&mut app);
    let e = EditState {
        loop_mode: LoopMode::Backward,
        ..s.edit().clone()
    };
    s.apply(e, None);
    assert_eq!(
        s.edit().crossfade,
        50,
        "kept for when the mode changes back"
    );
    let mut h = harness(app);

    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::S);
    h.run();
    assert!(h.state().ui.save_dialog.open);
    assert_eq!(
        h.query_all_by_label("Loop crossfade will be mixed into the audio")
            .count(),
        0
    );
    h.get_by_label("Save 1 file(s)").click();
    h.run();
    h.get_by_label("Close").click();
    h.run();
    assert!(!session(h.state_mut()).just_saved);
    assert_eq!(
        h.query_all_by_label("crossfade applied in saved file")
            .count(),
        0
    );
}

#[test]
fn save_as_dialog_saves_and_rebases() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_wav(dir.path(), "tone.wav");
    let mut h = harness(edited_app(source.clone()));
    assert_eq!(
        h.query_all_by_label("crossfade applied in saved file")
            .count(),
        0
    );

    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::S);
    h.run();
    assert!(h.state().ui.save_dialog.open);
    h.get_by_label("Individual WAV files");
    h.get_by_label("Overwrite originals");
    h.get_by_label("Keeps a .bak copy of each file it replaces");
    h.get_by_label("smpl chunk: root key, fine tune, loop points");
    h.get_by_label(&format!("Software tag: {}", software_tag()));
    h.get_by_label("Loop crossfade will be mixed into the audio");
    h.get_by_label("Trim after loop end, keep");
    h.get_by_label("Only save files that changed");
    h.get_by_label("changed");
    assert_eq!(
        h.query_all_by_label("All files in folder").count(),
        0,
        "a single file has no folder scope"
    );

    h.get_by_label("Save 1 file(s)").click();
    h.run();
    assert!(!h.state().ui.save_dialog.open);
    let written = dir.path().join("limewave").join("tone.wav");
    h.get_by_label("Save results");
    h.get_by_label(&format!("tone.wav: OK, saved to {}", written.display()));
    h.get_by_label("Close").click();
    h.run();
    assert!(h.state().save_results.is_none());

    let s = session(h.state_mut());
    assert_eq!(s.doc.path, written);
    assert_eq!(s.edit().crossfade, 0);
    assert!(!s.history.is_dirty());
    assert_eq!(
        s.destination,
        Some(Destination::Folder(dir.path().join("limewave")))
    );
    h.get_by_label("crossfade applied in saved file");

    // Ctrl+S now goes straight to the same folder.
    let s = session(h.state_mut());
    let e = EditState {
        root: 62,
        ..s.edit().clone()
    };
    s.apply(e, None);
    h.run();
    assert_eq!(
        h.query_all_by_label("crossfade applied in saved file")
            .count(),
        0
    );
    h.key_press_modifiers(Modifiers::COMMAND, Key::S);
    h.run();
    assert!(!h.state().ui.save_dialog.open);
    assert!(h.state().save_results.is_none());
    assert!(!session(h.state_mut()).history.is_dirty());
    assert_eq!(load_wav(&written).unwrap().initial_edit().root, 62);
    let names: Vec<_> = std::fs::read_dir(dir.path().join("limewave"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["tone.wav"],
        "its own saved file is replaced, no .bak"
    );
}

#[test]
fn saving_keeps_the_spectrogram_picture_up() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_wav(dir.path(), "tone.wav");
    let mut h = harness(edited_app(source));
    h.get_by_label("Spectrogram").click();
    h.run();
    let ctx = h.ctx.clone();
    block_until_ready(&mut session(h.state_mut()).spectrogram, &ctx);
    h.run();
    let shown = |h: &mut Harness<'static, LimewaveApp>| {
        let s = session(h.state_mut());
        picture_for(&s.spectrogram, s.frames(), s.processed.len()).is_some()
    };
    assert!(shown(&mut h), "the picture is up before the save");

    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::S);
    h.run();
    h.get_by_label("Save 1 file(s)").click();
    h.step();
    assert!(!h.state().ui.save_dialog.open, "saved");
    assert!(
        session(h.state_mut())
            .doc
            .path
            .starts_with(dir.path().join("limewave")),
        "the session was rebased on the saved file"
    );
    assert!(
        shown(&mut h),
        "the old picture stays up until the new one lands"
    );
}

#[test]
fn save_as_asks_before_replacing_another_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_wav(dir.path(), "tone.wav");
    let out = dir.path().join("limewave");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("tone.wav"), b"someone else's file").unwrap();
    let mut h = harness(edited_app(source));

    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::S);
    h.run();
    h.get_by_label(
        "1 file(s) already exist in this folder and will be replaced; each replaced file is kept as a .bak copy",
    );
    h.get_by_label("Save 1 file(s)").click();
    h.run();
    assert!(h.state().ui.save_dialog.open, "Save stays disabled");
    assert_eq!(
        std::fs::read(out.join("tone.wav")).unwrap(),
        b"someone else's file"
    );

    h.get_by_label("Replace existing files").click();
    h.run();
    h.get_by_label("Save 1 file(s)").click();
    h.run();
    assert!(!h.state().ui.save_dialog.open);
    assert!(
        load_wav(&out.join("tone.wav")).is_ok(),
        "replaced after the tick"
    );
    assert_eq!(
        std::fs::read(out.join("tone.wav.bak")).unwrap(),
        b"someone else's file",
        "the replaced file is kept as a backup"
    );
}

#[test]
fn toolbar_save_without_destination_opens_the_dialog_and_cancel_closes_it() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_wav(dir.path(), "tone.wav");
    let mut h = harness(edited_app(source.clone()));
    // The toolbar's Save comes before the inspector's.
    h.get_all_by_label("Save").next().unwrap().click();
    h.run();
    assert!(h.state().ui.save_dialog.open);
    h.get_by_label("Cancel").click();
    h.run();
    assert!(!h.state().ui.save_dialog.open);
    assert!(session(h.state_mut()).history.is_dirty());
    assert!(!dir.path().join("limewave").exists(), "nothing was written");
}

#[test]
fn unsaved_prompt_save_all_saves_then_opens() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_wav(dir.path(), "tone.wav");
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let mut app = edited_app(source);
    app.request(PendingAction::OpenFolder(empty));
    let mut h = harness(app);
    h.get_by_label("Unsaved changes");
    h.get_by_label("Save all").click();
    h.run();
    assert!(h.state().pending.is_none());
    assert!(h.state().files.is_empty(), "the folder opened after saving");
    assert!(dir.path().join("limewave").join("tone.wav").exists());
}
