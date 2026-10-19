//! The application: sessions, folder loading, panels and the unsaved-changes prompt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};
use seamlime_core::document::{WavDocument, load_wav};
use seamlime_core::dsp::{PeakPyramid, mono_sum};
use seamlime_core::edit::{EditState, History};
use seamlime_core::render::render;
use seamlime_core::save::Destination;

use crate::audio::engine::AudioEngine;
use crate::audio::voice::{PlayBuffer, play_buffer};
use crate::folder;
use crate::theme::{self, colors};
use crate::ui::coords::ViewTransform;
use crate::ui::filelist;
use crate::ui::overview::overview;
use crate::ui::waveform::waveform_view;

/// Longest a drag preview waits before the audio engine buffer is refreshed.
const PREVIEW_PUSH_INTERVAL: Duration = Duration::from_millis(50);

type LoadResult = (usize, Result<WavDocument, String>);

/// One open file: the decoded document, its edit history and the data derived from it.
pub struct Session {
    pub doc: WavDocument,
    pub history: History,
    /// One peak pyramid per channel of `doc.audio`.
    pub peaks: Vec<PeakPyramid>,
    /// Channel average of `doc.audio`, for analysis and drawing.
    pub mono: Vec<f32>,
    /// `doc.audio` with the current crossfade applied.
    pub rendered: Vec<Vec<f32>>,
    pub destination: Option<Destination>,
    /// Set when the file was just saved; cleared by the UI that reports it.
    pub just_saved: bool,
    /// What the waveform shows; `None` until the first draw fits the whole file.
    pub view: Option<ViewTransform>,
    /// The edit being dragged, not yet in the history.
    pub preview: Option<EditState>,
    /// When the engine buffer last followed a drag preview.
    last_preview_push: Option<Instant>,
}

impl Session {
    pub fn new(doc: WavDocument) -> Session {
        let initial = doc.initial_edit();
        let peaks = doc.audio.iter().map(|c| PeakPyramid::build(c)).collect();
        let mono = mono_sum(&doc.audio);
        let rendered = render(&doc.audio, &initial);
        Session {
            doc,
            history: History::new(initial),
            peaks,
            mono,
            rendered,
            destination: None,
            just_saved: false,
            view: None,
            preview: None,
            last_preview_push: None,
        }
    }

    pub fn edit(&self) -> &EditState {
        self.history.current()
    }

    /// The edit the views draw: the drag preview if there is one, else the current edit.
    pub fn shown_edit(&self) -> &EditState {
        self.preview.as_ref().unwrap_or_else(|| self.edit())
    }

    /// The buffer the audio engine plays for the current edit.
    pub fn play_buffer(&self) -> PlayBuffer {
        play_buffer(&self.doc.audio, self.doc.format.sample_rate, self.edit())
    }

    /// Records `e` (clamped to the file) as the new current edit.
    pub fn apply(&mut self, e: EditState, engine: Option<&AudioEngine>) {
        self.preview = None;
        self.history.push(e.clamped(self.doc.frames));
        self.refresh(engine);
    }

    pub fn undo(&mut self, engine: Option<&AudioEngine>) {
        self.preview = None;
        if self.history.undo() {
            self.refresh(engine);
        }
    }

    pub fn redo(&mut self, engine: Option<&AudioEngine>) {
        self.preview = None;
        if self.history.redo() {
            self.refresh(engine);
        }
    }

    /// Shows `e` (clamped to the file) while a marker is dragged. The rendered audio and the
    /// engine buffer follow at most every 50 ms; the history is untouched until
    /// [`Session::commit_preview`].
    pub fn set_preview(&mut self, e: EditState, engine: Option<&AudioEngine>) {
        let e = e.clamped(self.doc.frames);
        let due = self
            .last_preview_push
            .is_none_or(|t| t.elapsed() >= PREVIEW_PUSH_INTERVAL);
        if due {
            self.publish(&e, engine);
            self.last_preview_push = Some(Instant::now());
        }
        self.preview = Some(e);
    }

    /// Ends a drag: the preview becomes one history entry (if it changed anything) and the
    /// rendered audio and engine buffer are brought up to date.
    pub fn commit_preview(&mut self, engine: Option<&AudioEngine>) {
        self.last_preview_push = None;
        match self.preview.take() {
            Some(e) if e != *self.edit() => self.apply(e, engine),
            _ => self.refresh(engine),
        }
    }

    /// Recomputes `rendered` for the current edit and publishes it to the engine.
    fn refresh(&mut self, engine: Option<&AudioEngine>) {
        let e = self.history.current().clone();
        self.publish(&e, engine);
    }

    /// Renders `e` into `rendered` and, with an engine, publishes the matching play buffer.
    fn publish(&mut self, e: &EditState, engine: Option<&AudioEngine>) {
        match engine {
            Some(engine) => {
                let buffer = play_buffer(&self.doc.audio, self.doc.format.sample_rate, e);
                self.rendered.clone_from(&buffer.audio);
                engine.set_buffer(buffer);
            }
            None => self.rendered = render(&self.doc.audio, e),
        }
    }
}

/// A slot in the file list.
pub enum FileEntry {
    Loading(PathBuf),
    Loaded(Box<Session>),
    Failed(PathBuf, String),
}

impl FileEntry {
    pub fn path(&self) -> &Path {
        match self {
            FileEntry::Loading(p) | FileEntry::Failed(p, _) => p,
            FileEntry::Loaded(s) => &s.doc.path,
        }
    }

    pub fn file_name(&self) -> String {
        self.path()
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }

    fn is_dirty(&self) -> bool {
        matches!(self, FileEntry::Loaded(s) if s.history.is_dirty())
    }
}

/// View and tool settings that outlive a single frame.
pub struct UiState {
    pub snap: bool,
    pub lock_length: bool,
    pub audition: bool,
    pub octave_base: u8,
    pub held: BTreeSet<u8>,
}

impl Default for UiState {
    fn default() -> Self {
        UiState {
            snap: true,
            lock_length: false,
            audition: true,
            octave_base: 48,
            held: BTreeSet::new(),
        }
    }
}

/// Something that would throw away unsaved edits, held back until the user decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingAction {
    Close,
    OpenFile(PathBuf),
    OpenFolder(PathBuf),
}

pub struct SeamlimeApp {
    pub files: Vec<FileEntry>,
    pub selected: Option<usize>,
    pub engine: Option<AudioEngine>,
    pub audio_error: Option<String>,
    pub ui: UiState,
    /// The action waiting on the unsaved-changes prompt.
    pub pending: Option<PendingAction>,
    ctx: egui::Context,
    loader: Option<Receiver<LoadResult>>,
    single_file: bool,
    allow_close: bool,
}

impl SeamlimeApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let (engine, audio_error) = match AudioEngine::start() {
            Ok(engine) => (Some(engine), None),
            Err(e) => (None, Some(e)),
        };
        Self::build(cc.egui_ctx.clone(), engine, audio_error)
    }

    /// An app with no audio engine and no MIDI, for tests.
    pub fn for_test() -> Self {
        Self::build(egui::Context::default(), None, None)
    }

    fn build(ctx: egui::Context, engine: Option<AudioEngine>, audio_error: Option<String>) -> Self {
        SeamlimeApp {
            files: Vec::new(),
            selected: None,
            engine,
            audio_error,
            ui: UiState::default(),
            pending: None,
            ctx,
            loader: None,
            single_file: false,
            allow_close: false,
        }
    }

    /// Opens one file synchronously and hides the file list.
    pub fn open_file(&mut self, p: PathBuf) {
        let entry = match load_wav(&p) {
            Ok(doc) => FileEntry::Loaded(Box::new(Session::new(doc))),
            Err(e) => FileEntry::Failed(p, e.to_string()),
        };
        self.files = vec![entry];
        self.single_file = true;
        self.loader = None;
        self.select(0);
    }

    /// Lists the folder's WAV files and loads them in the background.
    pub fn open_folder(&mut self, dir: PathBuf) {
        let paths = folder::wav_files(&dir);
        self.files = paths.iter().cloned().map(FileEntry::Loading).collect();
        self.single_file = false;
        self.selected = None;
        self.loader = Some(folder::load_in_background(paths, self.ctx.clone()));
    }

    /// Runs `action`, or asks first when it would discard unsaved edits.
    pub fn request(&mut self, action: PendingAction) {
        if self.dirty_count() > 0 {
            self.pending = Some(action);
        } else {
            self.perform(action);
        }
    }

    /// Resolves the unsaved-changes prompt. Only `discard == true` runs the held action.
    pub fn resolve_pending(&mut self, discard: bool) {
        if let Some(action) = self.pending.take()
            && discard
        {
            self.perform(action);
        }
    }

    fn perform(&mut self, action: PendingAction) {
        match action {
            PendingAction::Close => {
                self.allow_close = true;
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            PendingAction::OpenFile(p) => self.open_file(p),
            PendingAction::OpenFolder(d) => self.open_folder(d),
        }
    }

    pub fn dirty_count(&self) -> usize {
        self.files.iter().filter(|f| f.is_dirty()).count()
    }

    /// Makes `i` the edited file and hands its sound to the audio engine.
    pub fn select(&mut self, i: usize) {
        self.selected = Some(i);
        if let (Some(FileEntry::Loaded(s)), Some(engine)) = (self.files.get(i), &self.engine) {
            engine.set_buffer(s.play_buffer());
        }
    }

    pub fn undo(&mut self) {
        let engine = self.engine.as_ref();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i)) {
            s.undo(engine);
        }
    }

    pub fn redo(&mut self) {
        let engine = self.engine.as_ref();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i)) {
            s.redo(engine);
        }
    }

    /// Moves finished background loads into the file list.
    fn drain_loader(&mut self) {
        let Some(rx) = &self.loader else { return };
        let mut results = Vec::new();
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(r) => results.push(r),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.loader = None;
        }
        for (i, result) in results {
            let Some(slot) = self.files.get_mut(i) else {
                continue;
            };
            *slot = match result {
                Ok(doc) => FileEntry::Loaded(Box::new(Session::new(doc))),
                Err(e) => FileEntry::Failed(slot.path().to_path_buf(), e),
            };
            if self.selected.is_none() && matches!(self.files[i], FileEntry::Loaded(_)) {
                self.select(i);
            }
        }
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|i| i.viewport().close_requested());
        if requested && !self.allow_close && self.dirty_count() > 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending = Some(PendingAction::Close);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() {
            return;
        }
        let redo_shift = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
        let redo_y = KeyboardShortcut::new(Modifiers::COMMAND, Key::Y);
        let undo = KeyboardShortcut::new(Modifiers::COMMAND, Key::Z);
        // The shift variant first: Ctrl+Shift+Z would otherwise also match plain Ctrl+Z.
        let (do_redo, do_undo) = ctx.input_mut(|i| {
            let redo = i.consume_shortcut(&redo_shift) | i.consume_shortcut(&redo_y);
            (redo, i.consume_shortcut(&undo))
        });
        if do_redo {
            self.redo();
        } else if do_undo {
            self.undo();
        }
    }

    /// Draws the whole window into `ui`.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.drain_loader();
        if self.loader.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        self.handle_close_request(&ctx);
        self.handle_shortcuts(&ctx);

        self.toolbar(ui);
        if !self.single_file {
            let mut clicked = None;
            egui::Panel::left("file_list")
                .resizable(true)
                .default_size(280.0)
                .size_range(200.0..=480.0)
                .show(ui, |ui| {
                    clicked = filelist::file_list(ui, &self.files, self.selected);
                });
            if let Some(i) = clicked {
                self.select(i);
            }
        }
        // Panel scaffolding: the inspector and keyboard strip are filled in by later tasks.
        egui::Panel::right("inspector")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=480.0)
            .show(ui, |_| {});
        egui::Panel::bottom("keyboard")
            .exact_size(84.0)
            .show(ui, |_| {});
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));
        self.unsaved_prompt(&ctx);
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        let mut undo = false;
        let mut redo = false;
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open file…").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("WAV audio", &["wav", "WAV"])
                        .pick_file()
                {
                    action = Some(PendingAction::OpenFile(p));
                }
                if ui.button("Open folder…").clicked()
                    && let Some(d) = rfd::FileDialog::new().pick_folder()
                {
                    action = Some(PendingAction::OpenFolder(d));
                }
                ui.separator();
                undo = ui.button("Undo").clicked();
                redo = ui.button("Redo").clicked();
                ui.separator();
                ui.toggle_value(&mut self.ui.snap, "Snap").on_hover_text(
                    "Snap loop markers to upward zero crossings (Shift disables while dragging)",
                );
                ui.toggle_value(&mut self.ui.lock_length, "Lock length")
                    .on_hover_text("Moving either loop marker moves both");
                ui.separator();
                self.status(ui);
            });
        });
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        if let Some(a) = action {
            self.request(a);
        }
    }

    fn status(&self, ui: &mut egui::Ui) {
        if let Some(err) = &self.audio_error {
            ui.colored_label(colors::END, format!("Audio unavailable: {err}"));
        } else if let Some(engine) = &self.engine {
            ui.weak(format!(
                "Audio: {} ({} Hz)",
                engine.device_name, engine.device_rate
            ));
        }
        if !self.single_file && !self.files.is_empty() {
            ui.weak(format!("{} files", self.files.len()));
        }
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        let engine = self.engine.as_ref();
        let ui_state = &mut self.ui;
        let Some(entry) = self.selected.and_then(|i| self.files.get_mut(i)) else {
            ui.weak("Open a WAV file or a folder to begin.");
            return;
        };
        ui.heading(entry.file_name());
        match entry {
            FileEntry::Loaded(s) => {
                ui.label(format!("{} frames", s.doc.frames));
                overview(ui, s);
                ui.add_space(4.0);
                // The waveform gets its own region; the seam view goes below it.
                let size = ui.available_size();
                ui.allocate_ui(size, |ui| waveform_view(ui, s, ui_state, engine));
            }
            FileEntry::Loading(_) => {
                ui.weak("Loading…");
            }
            FileEntry::Failed(_, e) => {
                ui.colored_label(colors::END, e.as_str());
            }
        }
    }

    fn unsaved_prompt(&mut self, ctx: &egui::Context) {
        if self.pending.is_none() {
            return;
        }
        let count = self.dirty_count();
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("unsaved_changes")).show(ctx, |ui| {
            ui.heading("Unsaved changes");
            ui.label(format!(
                "{count} file(s) have edits that have not been saved. Discarding them cannot be undone."
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Discard").clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if let Some(discard) = choice {
            self.resolve_pending(discard);
        }
    }
}

impl eframe::App for SeamlimeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seamlime_core::document::load_wav_bytes;
    use seamlime_core::edit::LoopMode;
    use seamlime_core::testwav::{sine, wav_bytes};
    use seamlime_core::wav::SampleFormat;

    fn session() -> Session {
        let bytes = wav_bytes(
            &[sine(220.0, 44100, 8000, 0.5)],
            44100,
            SampleFormat::Int { bits: 16 },
            Vec::new(),
        );
        Session::new(load_wav_bytes(PathBuf::from("t.wav"), &bytes).unwrap())
    }

    #[test]
    fn session_apply_updates_history_and_render() {
        let mut s = session();
        assert!(!s.history.is_dirty());
        let e = EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 2000,
            loop_end: 6000,
            crossfade: 10,
            ..s.edit().clone()
        };
        s.apply(e, None);
        assert!(s.history.is_dirty());
        // The crossfade replaces the 10 frames ending at the loop end.
        let region = 5991..=6000;
        assert!(
            region
                .clone()
                .any(|i| s.rendered[0][i] != s.doc.audio[0][i])
        );
        assert_eq!(s.rendered[0][..5991], s.doc.audio[0][..5991]);

        s.undo(None);
        assert!(!s.history.is_dirty());
        assert_eq!(s.rendered, s.doc.audio);
        s.redo(None);
        assert!(
            region
                .into_iter()
                .any(|i| s.rendered[0][i] != s.doc.audio[0][i])
        );
    }

    #[test]
    fn preview_is_throttled_and_commits_as_one_history_entry() {
        let mut s = session();
        let looped = |crossfade| EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 2000,
            loop_end: 6000,
            crossfade,
            ..s.edit().clone()
        };
        let (faded, plain) = (looped(10), looped(0));

        s.set_preview(faded.clone(), None);
        assert_eq!(s.shown_edit(), &faded);
        assert!(!s.history.is_dirty(), "a preview is not in the history");
        assert_ne!(
            s.rendered[0][5995], s.doc.audio[0][5995],
            "first preview renders"
        );

        // Within 50 ms the rendered audio is left alone, but the view follows at once.
        s.set_preview(plain.clone(), None);
        assert_eq!(s.shown_edit(), &plain);
        assert_ne!(s.rendered[0][5995], s.doc.audio[0][5995], "throttled");

        s.commit_preview(None);
        assert!(s.preview.is_none());
        assert_eq!(s.edit(), &plain);
        assert_eq!(s.rendered, s.doc.audio, "release renders the final edit");
        s.undo(None);
        assert!(!s.history.is_dirty(), "one drag, one undo step");

        // A drag that ends where it began adds nothing.
        s.set_preview(s.edit().clone(), None);
        s.commit_preview(None);
        assert!(!s.history.is_dirty());
        assert!(!s.history.undo());
    }

    #[test]
    fn dirty_sessions_hold_back_open_until_discard() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = SeamlimeApp::for_test();
        app.files = vec![FileEntry::Loaded(Box::new(session()))];
        app.select(0);
        app.request(PendingAction::OpenFolder(dir.path().to_path_buf()));
        assert!(app.pending.is_none());
        assert_eq!(app.files.len(), 0, "a clean session opens at once");

        app.files = vec![FileEntry::Loaded(Box::new(session()))];
        let FileEntry::Loaded(s) = &mut app.files[0] else {
            unreachable!()
        };
        let e = EditState {
            start: 5,
            ..s.edit().clone()
        };
        s.apply(e, None);
        assert_eq!(app.dirty_count(), 1);

        app.request(PendingAction::OpenFolder(dir.path().to_path_buf()));
        assert!(app.pending.is_some());
        assert_eq!(app.files.len(), 1);
        app.resolve_pending(false);
        assert!(app.pending.is_none());
        assert_eq!(app.dirty_count(), 1, "cancel keeps the edits");

        app.request(PendingAction::OpenFolder(dir.path().to_path_buf()));
        app.resolve_pending(true);
        assert_eq!(app.dirty_count(), 0);
        assert!(app.files.is_empty(), "discard opens the empty folder");
    }
}
