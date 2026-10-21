//! The save dialog, saving through `save_wav`, and rebasing a session on the saved file.

use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Button, DragValue, Grid, RadioButton, RichText, ScrollArea, TextEdit, Ui,
};
use limewave_core::document::load_wav;
use limewave_core::edit::{self, EditState};
use limewave_core::save::{Destination, SaveOutcome, default_folder, save_wav};
use limewave_core::software_tag;

use crate::app::{FileEntry, LimewaveApp, Session};
use crate::theme::colors;

/// One file's save result: the source path and what was written, or why not.
pub type SaveResult = (PathBuf, Result<SaveOutcome, String>);

/// A click on one of the Save buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveCommand {
    /// Save to the remembered destination, or ask through the dialog.
    Save,
    /// Always ask through the dialog.
    SaveAs,
}

/// What the save dialog shows and remembers between openings.
pub struct SaveDialogState {
    pub open: bool,
    pub overwrite: bool,
    pub folder: PathBuf,
    pub trim_after_loop: bool,
    pub padding: usize,
    pub only_changed: bool,
    /// Save every loaded file in the folder, not just `current`.
    pub scope_all: bool,
    /// The file the dialog was opened for.
    pub current: usize,
    /// Whether a folder is open, which offers `scope_all`.
    pub folder_mode: bool,
    /// The per-file "save" ticks, indexed like the app's file list.
    pub selected_files: Vec<bool>,
    /// The user agreed to replace other files of the same name in `folder`. Reset when the
    /// dialog opens or the folder changes.
    pub replace_existing: bool,
}

impl Default for SaveDialogState {
    fn default() -> Self {
        SaveDialogState {
            open: false,
            overwrite: false,
            folder: PathBuf::new(),
            trim_after_loop: false,
            padding: 16,
            only_changed: true,
            scope_all: false,
            current: 0,
            folder_mode: false,
            selected_files: Vec::new(),
            replace_existing: false,
        }
    }
}

/// The file a folder save of `s` into `dir` would replace, when that file exists and is
/// not `s`'s own file (a session reopened from its saved file replaces itself freely).
pub fn other_file_in_the_way(dir: &Path, s: &Session) -> Option<PathBuf> {
    let target = dir.join(s.doc.path.file_name()?);
    let own = match (target.canonicalize(), s.doc.path.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => target == s.doc.path,
    };
    (target.exists() && !own).then_some(target)
}

impl SaveDialogState {
    /// Opens the dialog for file `current`, starting from its remembered destination (or
    /// the default folder next to it). Does nothing unless that file is loaded.
    pub fn open_for(&mut self, current: usize, files: &[FileEntry], folder_mode: bool) {
        let Some(FileEntry::Loaded(s)) = files.get(current) else {
            return;
        };
        self.open = true;
        self.current = current;
        self.folder_mode = folder_mode;
        self.scope_all = false;
        // Spec §6.6: on by default in folder mode.
        self.only_changed = folder_mode;
        self.selected_files = vec![true; files.len()];
        self.overwrite = s.destination == Some(Destination::OverwriteWithBackup);
        self.replace_existing = false;
        self.folder = match &s.destination {
            Some(Destination::Folder(dir)) => dir.clone(),
            _ => default_folder(&s.doc.path),
        };
    }

    /// Changes the folder; any agreement to replace files there no longer holds.
    pub fn set_folder(&mut self, folder: PathBuf) {
        if folder != self.folder {
            self.folder = folder;
            self.replace_existing = false;
        }
    }

    /// The chosen files that would replace a different file of the same name in the new
    /// folder (none when overwriting originals).
    pub fn conflicts(&self, files: &[FileEntry]) -> Vec<PathBuf> {
        if self.overwrite {
            return Vec::new();
        }
        self.chosen(files)
            .into_iter()
            .filter_map(|i| match &files[i] {
                FileEntry::Loaded(s) => other_file_in_the_way(&self.folder, s),
                _ => None,
            })
            .collect()
    }

    /// The loaded files the table lists: the current one, or all with `scope_all`.
    fn scope<'a>(
        &self,
        files: &'a [FileEntry],
    ) -> impl Iterator<Item = (usize, &'a Session)> + use<'a> {
        let all = self.scope_all && self.folder_mode;
        let current = self.current;
        files.iter().enumerate().filter_map(move |(i, f)| match f {
            FileEntry::Loaded(s) if all || i == current => Some((i, s.as_ref())),
            _ => None,
        })
    }

    /// Whether the save box of a file can be ticked.
    fn can_save(&self, s: &Session) -> bool {
        !self.only_changed || s.history.is_dirty()
    }

    /// The files that will be saved.
    pub fn chosen(&self, files: &[FileEntry]) -> Vec<usize> {
        self.scope(files)
            .filter(|(i, s)| self.selected_files.get(*i) == Some(&true) && self.can_save(s))
            .map(|(i, _)| i)
            .collect()
    }

    /// Where the files go, or `None` while the folder path is unusable.
    fn destination(&self) -> Option<Destination> {
        if self.overwrite {
            Some(Destination::OverwriteWithBackup)
        } else if self.folder.is_absolute() {
            Some(Destination::Folder(self.folder.clone()))
        } else {
            None
        }
    }

    /// The request the Save button sends; `None` while it is disabled (nothing chosen, an
    /// unusable folder, or other files would be replaced without "Replace existing files").
    pub fn request(&self, files: &[FileEntry]) -> Option<SaveRequest> {
        let indices = self.chosen(files);
        if indices.is_empty() || (!self.replace_existing && !self.conflicts(files).is_empty()) {
            return None;
        }
        Some(SaveRequest {
            indices,
            destination: self.destination()?,
            trim_after_loop: self.trim_after_loop.then_some(self.padding),
        })
    }
}

/// Which files to save, where, and whether to trim after the loop end (the padding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRequest {
    pub indices: Vec<usize>,
    pub destination: Destination,
    pub trim_after_loop: Option<usize>,
}

/// Draws the save dialog while `st.open`. Returns the request when Save is clicked; Save,
/// Cancel, Escape and a click outside all close it.
pub fn save_dialog(
    ctx: &egui::Context,
    st: &mut SaveDialogState,
    app_files: &[FileEntry],
) -> Option<SaveRequest> {
    if !st.open {
        return None;
    }
    st.selected_files.resize(app_files.len(), true);
    let mut request = None;
    let mut cancel = false;
    let max_body = (ctx.content_rect().height() - 160.0).max(200.0);
    let modal = egui::Modal::new(egui::Id::unique("save_dialog")).show(ctx, |ui| {
        ui.set_width(540.0);
        ui.heading("Save as…");
        ui.separator();
        ScrollArea::vertical()
            .max_height(max_body)
            .show(ui, |ui| dialog_body(ui, st, app_files));
        ui.separator();
        let req = st.request(app_files);
        let count = st.chosen(app_files).len();
        ui.horizontal(|ui| {
            cancel = ui.button("Cancel").clicked();
            let save = ui.add_enabled(req.is_some(), Button::new(format!("Save {count} file(s)")));
            if save.clicked() {
                request = req;
            }
        });
    });
    if cancel || request.is_some() || modal.should_close() {
        st.open = false;
    }
    request
}

fn group(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong());
}

fn dialog_body(ui: &mut Ui, st: &mut SaveDialogState, files: &[FileEntry]) {
    group(ui, "Save as");
    let _ = ui.radio(true, "Individual WAV files");
    for label in ["SFZ instrument + its WAVs", "Both"] {
        ui.horizontal(|ui| {
            ui.add_enabled(false, RadioButton::new(false, label));
            ui.weak("coming later");
        });
    }

    group(ui, "Where");
    ui.horizontal(|ui| {
        let new_folder = ui.radio(!st.overwrite, "New folder");
        if new_folder.clicked() {
            st.overwrite = false;
        }
        ui.add_enabled_ui(!st.overwrite, |ui| {
            let mut text = st.folder.to_string_lossy().into_owned();
            let field = ui.add(TextEdit::singleline(&mut text).desired_width(300.0));
            let field = field.labelled_by(new_folder.id);
            if field.changed() {
                st.set_folder(PathBuf::from(text));
            }
            if ui.button("Browse…").clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_directory(&st.folder)
                    .pick_folder()
            {
                st.set_folder(dir);
            }
        });
    });
    if !st.overwrite && !st.folder.is_absolute() {
        ui.colored_label(colors::END, "Enter the full path of a folder");
    }
    let conflicts = st.conflicts(files);
    if !conflicts.is_empty() {
        ui.colored_label(
            colors::END,
            format!(
                "{} file(s) already exist in this folder and will be replaced; each replaced file is kept as a .bak copy",
                conflicts.len()
            ),
        );
        let names: Vec<String> = conflicts
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        ui.add(egui::Label::new(RichText::new(names.join(", ")).weak()).wrap());
        ui.checkbox(&mut st.replace_existing, "Replace existing files");
    }
    if ui.radio(st.overwrite, "Overwrite originals").clicked() {
        st.overwrite = true;
    }
    ui.weak("Keeps a .bak copy of each file it replaces");

    let chosen: Vec<&Session> = st
        .chosen(files)
        .into_iter()
        .filter_map(|i| match &files[i] {
            FileEntry::Loaded(s) => Some(s.as_ref()),
            _ => None,
        })
        .collect();

    group(ui, "Metadata");
    ui.label("smpl chunk: root key, fine tune, loop points");
    ui.label(format!("Software tag: {}", software_tag()));
    let markers = chosen.iter().filter(|s| s.doc.has_markers).count();
    if markers > 0 {
        ui.colored_label(
            colors::END,
            format!("Cue markers in {markers} files will be removed."),
        );
    }

    group(ui, "Audio changes");
    if chosen.iter().any(|s| mixes_crossfade(s.edit())) {
        ui.colored_label(colors::XFADE, "Loop crossfade will be mixed into the audio");
    }
    ui.horizontal(|ui| {
        let trim = ui.checkbox(&mut st.trim_after_loop, "Trim after loop end, keep");
        let padding = ui.add(DragValue::new(&mut st.padding).range(0..=1_000_000));
        let _ = padding.labelled_by(trim.id);
        ui.label("samples padding");
    });

    group(ui, "Files");
    if st.folder_mode {
        ui.checkbox(&mut st.scope_all, "All files in folder");
    }
    let rows: Vec<(usize, &Session)> = st.scope(files).collect();
    Grid::new("save_files")
        .num_columns(3)
        .spacing([16.0, 3.0])
        .show(ui, |ui| {
            ui.weak("File");
            ui.weak("Changes");
            ui.weak("Save");
            ui.end_row();
            for (i, s) in rows {
                let name = files[i].file_name();
                let name = ui.label(name);
                ui.label(if s.history.is_dirty() {
                    "changed"
                } else {
                    "unchanged"
                });
                let enabled = st.can_save(s);
                let mut ticked = st.selected_files[i] && enabled;
                let r = ui.add_enabled(enabled, egui::Checkbox::without_text(&mut ticked));
                let r = r.labelled_by(name.id);
                if r.changed() {
                    st.selected_files[i] = ticked;
                }
                ui.end_row();
            }
        });
    ui.checkbox(&mut st.only_changed, "Only save files that changed");
}

/// Whether saving `e` mixes a crossfade into the audio. A crossfade kept while the loop is
/// off does not count.
fn mixes_crossfade(e: &EditState) -> bool {
    e.crossfade.min(e.max_crossfade()) > 0
}

/// A session rebased onto its own file in `dir` saves over that file next time. `save_wav`
/// refuses that in folder mode (it guards originals), so it replaces the saved file
/// instead, with no backup. The original source is never in `dir` in this case: the
/// session only remembers `dir` after reopening the file it wrote there.
fn effective_destination(s: &Session, dest: &Destination) -> Destination {
    match dest {
        Destination::Folder(dir)
            if s.destination.as_ref() == Some(dest)
                && s.doc.path.file_name().map(|n| dir.join(n)).as_ref() == Some(&s.doc.path) =>
        {
            Destination::ReplaceSaved
        }
        _ => dest.clone(),
    }
}

/// Saves `s` to `dest` and rebases it on the written file. A failed save leaves `s` as it
/// was. If the file was written but cannot be reopened, `s` is kept, marked saved, and the
/// reload error is returned.
fn save_session(
    s: &mut Session,
    dest: &Destination,
    trim_after_loop: Option<usize>,
) -> Result<SaveOutcome, String> {
    let frames = s.doc.frames;
    let mut e = s.edit().clone();
    if let Some(padding) = trim_after_loop {
        e = edit::trim_after_loop(&e, padding, frames);
    }
    let crossfaded = mixes_crossfade(&e.clone().clamped(frames));
    let outcome =
        save_wav(&s.doc, &e, &effective_destination(s, dest)).map_err(|err| err.to_string())?;
    match load_wav(&outcome.written) {
        Ok(doc) => {
            let mut rebased = Session::new(doc);
            if rebased.doc.frames == frames {
                rebased.view = s.view;
            }
            rebased.destination = Some(dest.clone());
            rebased.just_saved = crossfaded;
            rebased.history.mark_saved();
            *s = rebased;
            Ok(outcome)
        }
        Err(err) => {
            s.history.mark_saved();
            s.destination = Some(dest.clone());
            Err(format!(
                "saved to {}, but could not reopen it: {err}",
                outcome.written.display()
            ))
        }
    }
}

/// Saves the files of `req` one by one, rebasing each saved session on its new file, and
/// hands the selected file's new sound to the audio engine. Only `save_wav` writes.
pub fn perform_save(app: &mut LimewaveApp, req: SaveRequest) -> Vec<SaveResult> {
    let mut results = Vec::with_capacity(req.indices.len());
    for &i in &req.indices {
        let Some(entry) = app.files.get_mut(i) else {
            continue;
        };
        let FileEntry::Loaded(s) = entry else {
            results.push((entry.path().to_path_buf(), Err("not loaded".to_owned())));
            continue;
        };
        let source = s.doc.path.clone();
        let result = save_session(s, &req.destination, req.trim_after_loop);
        if result.is_ok()
            && app.selected == Some(i)
            && let Some(engine) = &app.engine
        {
            engine.set_buffer(s.play_buffer());
        }
        results.push((source, result));
    }
    results
}

/// Lists what a save did, file by file. Returns true when the user closes it.
pub fn save_results_window(ctx: &egui::Context, results: &[SaveResult]) -> bool {
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::unique("save_results")).show(ctx, |ui| {
        ui.set_max_width(640.0);
        ui.heading("Save results");
        ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            for (path, result) in results {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                match result {
                    Ok(o) => {
                        ui.label(format!("{name}: OK, saved to {}", o.written.display()));
                        if let Some(backup) = &o.backup {
                            ui.weak(format!("Backup: {}", backup.display()));
                        }
                    }
                    Err(err) => {
                        ui.colored_label(colors::END, format!("{name}: {err}"));
                    }
                }
            }
        });
        ui.add_space(6.0);
        close = ui.button("Close").clicked();
    });
    close || modal.should_close()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use limewave_core::document::load_wav;
    use limewave_core::edit::LoopMode;
    use limewave_core::riff::Chunk;
    use limewave_core::smpl::{Smpl, SmplLoop};
    use limewave_core::testwav::{sine, wav_bytes};
    use limewave_core::wav::SampleFormat;

    use crate::app::Session;
    use crate::ui::coords::ViewTransform;

    const LSB: f32 = 1.0 / 32768.0;

    fn smpl_chunk(start: u32, end: u32) -> Chunk {
        let smpl = Smpl {
            manufacturer: 0,
            product: 0,
            sample_period: 22_675,
            unity_note: 60,
            pitch_fraction: 0,
            smpte_format: 0,
            smpte_offset: 0,
            loops: vec![SmplLoop {
                cue_id: 0,
                loop_type: 0,
                start,
                end,
                fraction: 0,
                play_count: 0,
            }],
            sampler_data: Vec::new(),
        };
        Chunk {
            id: *b"smpl",
            data: smpl.to_bytes(),
        }
    }

    /// Writes a 16-bit mono 440 Hz sine of 8000 frames, looped 1000..=5000, to `dir/name`.
    fn write_looped(dir: &Path, name: &str) -> PathBuf {
        let bytes = wav_bytes(
            &[sine(440.0, 44100, 8000, 0.5)],
            44100,
            SampleFormat::Int { bits: 16 },
            vec![smpl_chunk(1000, 5000)],
        );
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// An app holding `paths` as loaded files, the first one selected.
    fn app_with(paths: &[PathBuf]) -> LimewaveApp {
        let mut app = LimewaveApp::for_test();
        app.files = paths
            .iter()
            .map(|p| FileEntry::Loaded(Box::new(Session::new(load_wav(p).unwrap()))))
            .collect();
        app.select(0);
        app
    }

    fn session(app: &mut LimewaveApp, i: usize) -> &mut Session {
        match &mut app.files[i] {
            FileEntry::Loaded(s) => s,
            _ => panic!("file {i} is not loaded"),
        }
    }

    fn edit_with(s: &Session, f: impl FnOnce(&mut EditState)) -> EditState {
        let mut e = s.edit().clone();
        f(&mut e);
        e
    }

    fn folder_request(indices: Vec<usize>, dir: &Path) -> SaveRequest {
        SaveRequest {
            indices,
            destination: Destination::Folder(dir.to_path_buf()),
            trim_after_loop: None,
        }
    }

    #[test]
    fn perform_save_rebases_session() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let out = dir.path().join("out");
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.crossfade = 50);
        s.apply(e, None);
        assert_eq!(s.edit().crossfade, 50);
        let rendered = s.rendered.clone();
        assert_ne!(rendered, s.doc.audio, "the crossfade changes the audio");
        let view = ViewTransform {
            first: 900.0,
            frames_per_px: 2.0,
        };
        s.view = Some(view);

        let results = perform_save(&mut app, folder_request(vec![0], &out));
        assert_eq!(results.len(), 1);
        let (path, result) = &results[0];
        assert_eq!(path, &source);
        let outcome = result.as_ref().unwrap();
        assert_eq!(outcome.written, out.join("tone.wav"));
        assert_eq!(outcome.backup, None);

        let s = session(&mut app, 0);
        assert_eq!(s.doc.path, out.join("tone.wav"));
        assert_eq!(s.edit().crossfade, 0);
        assert_eq!((s.edit().loop_start, s.edit().loop_end), (1000, 5000));
        assert!(!s.history.is_dirty());
        assert!(s.just_saved, "the saved edit had a crossfade");
        assert_eq!(s.destination, Some(Destination::Folder(out.clone())));
        assert_eq!(s.doc.audio.len(), rendered.len());
        for (saved, before) in s.doc.audio.iter().zip(&rendered) {
            assert_eq!(saved.len(), before.len());
            for (i, (a, b)) in saved.iter().zip(before).enumerate() {
                assert!((a - b).abs() <= LSB, "frame {i}: {a} vs {b}");
            }
        }
        assert_eq!(s.rendered, s.doc.audio, "no crossfade left to render");
        assert_eq!(s.view, Some(view), "same length: the view stays");
        assert!(source.exists(), "the source is untouched");
    }

    #[test]
    fn perform_save_reports_errors_without_touching_session() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let original = std::fs::read(&source).unwrap();
        // A file where the destination folder should be: nothing can be written there.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"a file, not a folder").unwrap();
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| {
            e.crossfade = 50;
            e.root = 62;
        });
        s.apply(e.clone(), None);
        let rendered = s.rendered.clone();

        let results = perform_save(&mut app, folder_request(vec![0], &blocker));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, source);
        assert!(results[0].1.is_err());

        let s = session(&mut app, 0);
        assert_eq!(s.doc.path, source);
        assert_eq!(s.edit(), &e);
        assert!(s.history.is_dirty());
        assert!(!s.just_saved);
        assert_eq!(s.destination, None);
        assert_eq!(s.rendered, rendered);
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }

    #[test]
    fn trim_after_loop_sets_end() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let out = dir.path().join("out");
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let start = 100;
        let e = edit_with(s, |e| e.start = start);
        s.apply(e, None);
        let loop_end = s.edit().loop_end;
        s.view = Some(ViewTransform::fit(s.doc.frames, 800.0));

        let req = SaveRequest {
            trim_after_loop: Some(16),
            ..folder_request(vec![0], &out)
        };
        let results = perform_save(&mut app, req);
        assert!(results[0].1.is_ok(), "{:?}", results[0].1);

        let saved = load_wav(&out.join("tone.wav")).unwrap();
        assert_eq!(saved.frames, loop_end + 17 - start);
        let e = saved.initial_edit();
        assert_eq!(e.loop_mode, LoopMode::Forward);
        assert_eq!((e.loop_start, e.loop_end), (1000 - start, loop_end - start));
        let s = session(&mut app, 0);
        assert_eq!(s.doc.frames, loop_end + 17 - start);
        assert_eq!(s.view, None, "a new length refits on the next draw");
    }

    #[test]
    fn overwrite_rebases_on_the_original_and_keeps_backups() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let original = std::fs::read(&source).unwrap();
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.root = 62);
        s.apply(e, None);

        let overwrite = SaveRequest {
            indices: vec![0],
            destination: Destination::OverwriteWithBackup,
            trim_after_loop: None,
        };
        let results = perform_save(&mut app, overwrite.clone());
        let outcome = results[0].1.as_ref().unwrap();
        let backup = dir.path().join("tone.wav.bak");
        assert_eq!(outcome.written, source);
        assert_eq!(outcome.backup.as_ref(), Some(&backup));
        assert_eq!(std::fs::read(&backup).unwrap(), original);

        let s = session(&mut app, 0);
        assert_eq!(s.doc.path, source);
        assert_eq!(s.edit().root, 62, "the saved root key is read back");
        assert!(!s.history.is_dirty());
        assert!(!s.just_saved, "no crossfade was saved");
        assert_eq!(s.destination, Some(Destination::OverwriteWithBackup));

        // Save again: a second, numbered backup; the first one is kept.
        let e = edit_with(s, |e| e.root = 64);
        s.apply(e, None);
        let results = perform_save(&mut app, overwrite);
        let outcome = results[0].1.as_ref().unwrap();
        assert_eq!(
            outcome.backup.as_ref(),
            Some(&dir.path().join("tone.wav.1.bak"))
        );
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        assert_eq!(load_wav(&source).unwrap().initial_edit().root, 64);
    }

    #[test]
    fn resave_to_the_remembered_folder_replaces_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let original = std::fs::read(&source).unwrap();
        let out = dir.path().join("out");
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.root = 62);
        s.apply(e, None);
        assert!(
            perform_save(&mut app, folder_request(vec![0], &out))[0]
                .1
                .is_ok()
        );

        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.root = 64);
        s.apply(e, None);
        let results = perform_save(&mut app, folder_request(vec![0], &out));
        let outcome = results[0].1.as_ref().unwrap();
        assert_eq!(outcome.written, out.join("tone.wav"));
        assert_eq!(outcome.backup, None, "its own saved file needs no backup");
        assert_eq!(
            load_wav(&out.join("tone.wav")).unwrap().initial_edit().root,
            64
        );
        let names: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["tone.wav"]);
        let s = session(&mut app, 0);
        assert!(!s.history.is_dirty());
        assert_eq!(s.destination, Some(Destination::Folder(out.clone())));
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }

    #[test]
    fn new_folder_mode_never_writes_over_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let original = std::fs::read(&source).unwrap();
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.root = 62);
        s.apply(e, None);

        let results = perform_save(&mut app, folder_request(vec![0], dir.path()));
        assert!(results[0].1.is_err());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(session(&mut app, 0).history.is_dirty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn just_saved_clears_on_the_next_edit() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.crossfade = 50);
        s.apply(e, None);
        perform_save(&mut app, folder_request(vec![0], &dir.path().join("out")));
        let s = session(&mut app, 0);
        assert!(s.just_saved);
        let e = edit_with(s, |e| e.root = 61);
        s.apply(e, None);
        assert!(!s.just_saved);
    }

    #[test]
    fn a_crossfade_with_the_loop_off_is_not_mixed_in() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.crossfade = 50);
        s.apply(e, None);
        assert!(mixes_crossfade(s.edit()));
        let e = edit_with(s, |e| e.loop_mode = LoopMode::Off);
        s.apply(e, None);
        assert_eq!(s.edit().crossfade, 50, "kept for when the loop comes back");
        assert!(!mixes_crossfade(s.edit()));

        perform_save(&mut app, folder_request(vec![0], &dir.path().join("out")));
        let s = session(&mut app, 0);
        assert!(!s.history.is_dirty());
        assert!(!s.just_saved, "nothing was mixed into the saved file");
    }

    #[test]
    fn dialog_choice_follows_scope_ticks_and_only_changed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = [
            write_looped(dir.path(), "a.wav"),
            write_looped(dir.path(), "b.wav"),
        ];
        let mut app = app_with(&paths);
        let s = session(&mut app, 1);
        let e = edit_with(s, |e| e.root = 62);
        s.apply(e, None);

        let mut st = SaveDialogState::default();
        st.open_for(0, &app.files, true);
        assert!(st.open && st.only_changed && !st.scope_all && !st.overwrite);
        assert_eq!(st.folder, dir.path().join("limewave"));
        assert!(st.chosen(&app.files).is_empty(), "a.wav is clean");
        assert_eq!(st.request(&app.files), None);

        st.scope_all = true;
        assert_eq!(st.chosen(&app.files), vec![1]);
        st.only_changed = false;
        assert_eq!(st.chosen(&app.files), vec![0, 1]);
        st.selected_files[1] = false;
        assert_eq!(st.chosen(&app.files), vec![0]);

        st.trim_after_loop = true;
        assert_eq!(
            st.request(&app.files),
            Some(SaveRequest {
                indices: vec![0],
                destination: Destination::Folder(dir.path().join("limewave")),
                trim_after_loop: Some(16),
            })
        );
        st.folder = PathBuf::from("relative");
        assert_eq!(st.request(&app.files), None, "a relative folder is refused");
        st.overwrite = true;
        assert_eq!(
            st.request(&app.files).map(|r| r.destination),
            Some(Destination::OverwriteWithBackup)
        );

        // A single open file: Save as… saves it even when unchanged.
        let mut st = SaveDialogState::default();
        st.open_for(0, &app.files, false);
        assert!(!st.only_changed);
        st.scope_all = true;
        assert_eq!(st.chosen(&app.files), vec![0], "no folder, no scope_all");
    }

    #[test]
    fn dialog_starts_from_the_remembered_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let mut app = app_with(std::slice::from_ref(&source));
        let elsewhere = dir.path().join("elsewhere");
        session(&mut app, 0).destination = Some(Destination::Folder(elsewhere.clone()));
        let mut st = SaveDialogState::default();
        st.open_for(0, &app.files, false);
        assert_eq!((st.overwrite, &st.folder), (false, &elsewhere));
        session(&mut app, 0).destination = Some(Destination::OverwriteWithBackup);
        st.open_for(0, &app.files, false);
        assert!(st.overwrite);
    }

    #[test]
    fn save_goes_straight_to_the_remembered_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "tone.wav");
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.root = 62);
        s.apply(e, None);

        app.save();
        assert!(app.ui.save_dialog.open, "no destination yet: ask");
        assert_eq!(session(&mut app, 0).doc.path, source);
        app.ui.save_dialog.open = false;

        let out = dir.path().join("out");
        session(&mut app, 0).destination = Some(Destination::Folder(out.clone()));
        app.save();
        assert!(!app.ui.save_dialog.open);
        assert!(app.save_results.is_none(), "success needs no window");
        assert_eq!(session(&mut app, 0).doc.path, out.join("tone.wav"));
        assert!(!session(&mut app, 0).history.is_dirty());
    }

    #[test]
    fn save_all_saves_dirty_files_then_runs_the_held_action() {
        use crate::app::PendingAction;
        let dir = tempfile::tempdir().unwrap();
        let paths = [
            write_looped(dir.path(), "a.wav"),
            write_looped(dir.path(), "b.wav"),
            write_looped(dir.path(), "c.wav"),
        ];
        let mut app = app_with(&paths);
        for i in [0, 2] {
            let s = session(&mut app, i);
            let e = edit_with(s, |e| e.root = 62);
            s.apply(e, None);
        }
        let remembered = dir.path().join("remembered");
        session(&mut app, 2).destination = Some(Destination::Folder(remembered.clone()));
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();

        app.request(PendingAction::OpenFolder(empty));
        assert!(app.pending.is_some());
        app.resolve_pending_save_all();
        assert!(app.pending.is_none());
        assert!(app.save_results.is_none());
        assert!(app.files.is_empty(), "the held action ran");
        let default = dir.path().join("limewave");
        assert_eq!(
            load_wav(&default.join("a.wav"))
                .unwrap()
                .initial_edit()
                .root,
            62
        );
        assert!(!default.join("b.wav").exists(), "b.wav had no edits");
        assert!(remembered.join("c.wav").exists());
    }

    #[test]
    fn failed_save_all_keeps_everything_and_shows_results() {
        use crate::app::PendingAction;
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "a.wav");
        // A file named like the default folder: the save cannot create the folder.
        std::fs::write(dir.path().join("limewave"), b"not a folder").unwrap();
        let mut app = app_with(std::slice::from_ref(&source));
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.root = 62);
        s.apply(e, None);
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();

        app.request(PendingAction::OpenFolder(empty));
        app.resolve_pending_save_all();
        assert!(app.pending.is_none(), "the action is dropped");
        assert_eq!(app.files.len(), 1, "the folder was not opened");
        assert_eq!(app.dirty_count(), 1);
        let results = app.save_results.as_ref().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, source);
        assert!(results[0].1.is_err());
    }

    #[test]
    fn dialog_blocks_replacing_other_files_until_ticked() {
        let dir = tempfile::tempdir().unwrap();
        let paths = [
            write_looped(dir.path(), "a.wav"),
            write_looped(dir.path(), "b.wav"),
        ];
        let mut app = app_with(&paths);
        let out = dir.path().join("limewave");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("a.wav"), b"unrelated").unwrap();

        let mut st = SaveDialogState::default();
        st.open_for(0, &app.files, true);
        st.scope_all = true;
        st.only_changed = false;
        assert_eq!(st.conflicts(&app.files), vec![out.join("a.wav")]);
        assert_eq!(st.request(&app.files), None, "blocked until ticked");
        st.replace_existing = true;
        assert_eq!(st.request(&app.files).unwrap().indices, vec![0, 1]);

        // A new folder takes the agreement back; so does reopening the dialog.
        st.set_folder(dir.path().join("other"));
        assert!(!st.replace_existing);
        assert!(st.conflicts(&app.files).is_empty());
        st.set_folder(out.clone());
        assert_eq!(st.request(&app.files), None);
        st.replace_existing = true;
        st.open_for(0, &app.files, true);
        assert!(!st.replace_existing);

        // Overwriting originals replaces nothing in the folder.
        st.scope_all = true;
        st.only_changed = false;
        st.overwrite = true;
        assert!(st.conflicts(&app.files).is_empty());
        assert!(st.request(&app.files).is_some());

        // A session reopened from its own saved file is not in its own way.
        st.overwrite = false;
        std::fs::remove_file(out.join("a.wav")).unwrap();
        let s = session(&mut app, 0);
        let e = edit_with(s, |e| e.root = 62);
        s.apply(e, None);
        assert!(
            perform_save(&mut app, folder_request(vec![0], &out))[0]
                .1
                .is_ok()
        );
        assert_eq!(session(&mut app, 0).doc.path, out.join("a.wav"));
        assert!(st.conflicts(&app.files).is_empty());
    }

    #[test]
    fn replacing_a_different_file_keeps_it_as_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_looped(dir.path(), "a.wav");
        let out = dir.path().join("limewave");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("a.wav"), b"unrelated").unwrap();
        let mut app = app_with(std::slice::from_ref(&source));
        let mut st = SaveDialogState::default();
        st.open_for(0, &app.files, false);
        st.replace_existing = true;
        let req = st.request(&app.files).unwrap();

        let results = perform_save(&mut app, req);
        let outcome = results[0].1.as_ref().unwrap();
        assert_eq!(outcome.written, out.join("a.wav"));
        assert_eq!(outcome.backup, Some(out.join("a.wav.bak")));
        assert_eq!(std::fs::read(out.join("a.wav.bak")).unwrap(), b"unrelated");
        assert_eq!(session(&mut app, 0).doc.path, out.join("a.wav"));
    }

    #[test]
    fn save_all_refuses_to_replace_a_different_file() {
        use crate::app::PendingAction;
        let dir = tempfile::tempdir().unwrap();
        let paths = [
            write_looped(dir.path(), "a.wav"),
            write_looped(dir.path(), "b.wav"),
        ];
        let out = dir.path().join("limewave");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("a.wav"), b"unrelated").unwrap();
        let mut app = app_with(&paths);
        for i in [0, 1] {
            let s = session(&mut app, i);
            let e = edit_with(s, |e| e.root = 62);
            s.apply(e, None);
        }
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();

        app.request(PendingAction::OpenFolder(empty));
        app.resolve_pending_save_all();
        assert_eq!(app.files.len(), 2, "the folder was not opened");
        assert_eq!(std::fs::read(out.join("a.wav")).unwrap(), b"unrelated");
        assert_eq!(
            load_wav(&out.join("b.wav")).unwrap().initial_edit().root,
            62
        );
        assert!(session(&mut app, 0).history.is_dirty());
        assert!(!session(&mut app, 1).history.is_dirty());

        let results = app.save_results.as_ref().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, paths[0]);
        let want = format!(
            "a different file with this name already exists in {}; use Save as…",
            out.display()
        );
        assert_eq!(results[0].1, Err(want));
        assert!(results[1].1.is_ok());

        // After a save into a folder, Save all replaces that same file again.
        let s = session(&mut app, 1);
        let e = edit_with(s, |e| e.root = 64);
        s.apply(e, None);
        app.save_results = None;
        std::fs::remove_file(out.join("a.wav")).unwrap();
        assert!(app.save_all());
        assert_eq!(
            load_wav(&out.join("b.wav")).unwrap().initial_edit().root,
            64
        );
    }
}
