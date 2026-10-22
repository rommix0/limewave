use std::path::{Path, PathBuf};
use std::time::Duration;

use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, PendingAction};
use limewave_core::edit::EditState;
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

fn write_wav(dir: &Path, name: &str) -> PathBuf {
    let bytes = wav_bytes(
        &[sine(440.0, 44100, 4410, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn harness(app: LimewaveApp) -> Harness<'static, LimewaveApp> {
    Harness::builder()
        .with_size(eframe::egui::vec2(1100.0, 700.0))
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app)
}

#[test]
fn opens_file_and_shows_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_wav(dir.path(), "tone_single.wav");
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    let mut harness = harness(app);
    harness.run();
    harness.get_by_label_contains("tone_single.wav");
    harness.get_by_label_contains("4410 frames");
}

#[test]
fn folder_mode_lists_files_with_chips() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(dir.path(), "alpha.wav");
    write_wav(dir.path(), "beta.wav");
    std::fs::write(dir.path().join("broken.wav"), b"not a wav").unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_folder(dir.path().to_path_buf());
    let mut harness = harness(app);
    for _ in 0..400 {
        harness.step();
        let all_done = harness
            .state()
            .files
            .iter()
            .all(|f| !matches!(f, FileEntry::Loading(_)));
        if all_done {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.run();
    // The selected file also appears in the central heading, so its name matches twice.
    assert!(harness.query_all_by_label_contains("alpha.wav").count() >= 1);
    assert_eq!(harness.query_all_by_label_contains("beta.wav").count(), 1);
    assert_eq!(harness.query_all_by_label_contains("broken.wav").count(), 1);
    assert_eq!(harness.query_all_by_label("failed").count(), 1);
    assert_eq!(harness.query_all_by_label("no loop").count(), 2);
    assert_eq!(harness.query_all_by_label("C4").count(), 2);
}

#[test]
fn unsaved_prompt_offers_discard_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(write_wav(dir.path(), "edited.wav"));
    let FileEntry::Loaded(s) = &mut app.files[0] else {
        panic!("file did not load");
    };
    let e = EditState {
        start: 100,
        ..s.edit().clone()
    };
    s.apply(e, None);
    app.request(PendingAction::OpenFolder(dir.path().to_path_buf()));

    let mut harness = harness(app);
    harness.run();
    harness.get_by_label("Unsaved changes");
    harness.get_by_label("Cancel").click();
    harness.run();
    assert!(harness.state().pending.is_none());
    assert_eq!(harness.state().dirty_count(), 1);
    assert_eq!(harness.query_all_by_label("Unsaved changes").count(), 0);
}

#[test]
fn the_record_button_is_disabled_without_input_devices() {
    let app = LimewaveApp::for_test();
    assert_eq!(app.input_device_count, 0);
    let mut h = harness(app);
    h.run();
    assert!(h.get_by_label("Record").accesskit_node().is_disabled());
}
