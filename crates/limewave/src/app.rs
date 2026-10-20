//! The application: sessions, folder loading, panels and the unsaved-changes prompt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, KeyboardShortcut, Modifiers};
use limewave_core::document::{WavDocument, load_wav};
use limewave_core::dsp::{PeakPyramid, mono_sum};
use limewave_core::edit::{EditState, History};
use limewave_core::notes::note_name;
use limewave_core::render::render;
use limewave_core::save::{Destination, default_folder};

use crate::audio::engine::{AudioEngine, Command};
use crate::audio::keys::{octave_shift, qwerty_note};
use crate::audio::midi::{MidiConnection, connect_midi, list_midi_ports};
use crate::audio::voice::{PlayBuffer, play_buffer};
use crate::folder;
use crate::theme::{self, colors};
use crate::ui::coords::ViewTransform;
use crate::ui::filelist;
use crate::ui::inspector::inspector;
use crate::ui::keyboard::keyboard_strip;
use crate::ui::overview::overview;
use crate::ui::save_dialog::{
    SaveCommand, SaveDialogState, SaveRequest, SaveResult, other_file_in_the_way, perform_save,
    save_dialog, save_results_window,
};
use crate::ui::seam::{DEFAULT_HALF, SeamChannel, seam_view};
use crate::ui::waveform::waveform_view;

/// Longest a drag preview waits before the audio engine buffer is refreshed.
const PREVIEW_PUSH_INTERVAL: Duration = Duration::from_millis(50);

const SAVE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const SAVE_AS_SHORTCUT: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S);

/// Default height of the seam panel: its controls plus a plot of about 200 px.
const SEAM_PANEL_HEIGHT: f32 = 300.0;

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
    /// Set by a save that mixed a crossfade into the file (the inspector says so); cleared
    /// by the next edit, undo or redo.
    pub just_saved: bool,
    /// What the waveform shows; `None` until the first draw fits the whole file.
    pub view: Option<ViewTransform>,
    /// The edit being dragged, not yet in the history.
    pub preview: Option<EditState>,
    /// When the engine buffer last followed a drag preview.
    last_preview_push: Option<Instant>,
    /// The drag preview was throttled and is not yet in `rendered` or the engine.
    preview_held: bool,
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
            preview_held: false,
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
        self.just_saved = false;
        self.preview = None;
        self.history.push(e.clamped(self.doc.frames));
        self.refresh(engine);
    }

    pub fn undo(&mut self, engine: Option<&AudioEngine>) {
        self.just_saved = false;
        self.preview = None;
        if self.history.undo() {
            self.refresh(engine);
        }
    }

    pub fn redo(&mut self, engine: Option<&AudioEngine>) {
        self.just_saved = false;
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
        self.preview_held = !due;
        self.preview = Some(e);
    }

    /// Publishes a drag preview that `set_preview` held back once its wait is over, so the
    /// sound follows a drag that stops moving. Returns how long to wait when the preview is
    /// still held back (the caller asks for a repaint then), else `None`.
    pub fn flush_preview(&mut self, engine: Option<&AudioEngine>) -> Option<Duration> {
        if !self.preview_held {
            return None;
        }
        let Some(e) = self.preview.clone() else {
            self.preview_held = false;
            return None;
        };
        let elapsed = self
            .last_preview_push
            .map_or(PREVIEW_PUSH_INTERVAL, |t| t.elapsed());
        if elapsed < PREVIEW_PUSH_INTERVAL {
            return Some(PREVIEW_PUSH_INTERVAL - elapsed);
        }
        self.publish(&e, engine);
        self.last_preview_push = Some(Instant::now());
        self.preview_held = false;
        None
    }

    /// Ends a drag: the preview becomes one history entry (if it changed anything) and the
    /// rendered audio and engine buffer are brought up to date.
    pub fn commit_preview(&mut self, engine: Option<&AudioEngine>) {
        self.just_saved = false;
        self.last_preview_push = None;
        self.preview_held = false;
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
    /// Samples shown on each side of the seam.
    pub seam_half: usize,
    pub seam_channel: SeamChannel,
    pub save_dialog: SaveDialogState,
}

impl Default for UiState {
    fn default() -> Self {
        UiState {
            snap: true,
            lock_length: false,
            audition: true,
            octave_base: 48,
            held: BTreeSet::new(),
            seam_half: DEFAULT_HALF,
            seam_channel: SeamChannel::default(),
            save_dialog: SaveDialogState::default(),
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

pub struct LimewaveApp {
    pub files: Vec<FileEntry>,
    pub selected: Option<usize>,
    pub engine: Option<AudioEngine>,
    pub audio_error: Option<String>,
    pub ui: UiState,
    /// The action waiting on the unsaved-changes prompt.
    pub pending: Option<PendingAction>,
    /// The results of the last save, shown until the user closes them.
    pub save_results: Option<Vec<SaveResult>>,
    /// The selected file's Start frame, read by the MIDI thread for each note-on.
    start_frame: Arc<AtomicUsize>,
    midi: Option<MidiConnection>,
    /// The MIDI command producer while no connection holds it.
    midi_spare: Option<rtrb::Producer<Command>>,
    midi_ports: Vec<String>,
    midi_selected: Option<usize>,
    midi_error: Option<String>,
    /// True while the engine has been silenced because no loaded file is selected.
    silenced: bool,
    ctx: egui::Context,
    loader: Option<Receiver<LoadResult>>,
    single_file: bool,
    allow_close: bool,
}

impl LimewaveApp {
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

    fn build(
        ctx: egui::Context,
        mut engine: Option<AudioEngine>,
        audio_error: Option<String>,
    ) -> Self {
        let midi_spare = engine.as_mut().and_then(AudioEngine::midi_sender);
        LimewaveApp {
            files: Vec::new(),
            selected: None,
            engine,
            audio_error,
            ui: UiState::default(),
            pending: None,
            save_results: None,
            start_frame: Arc::new(AtomicUsize::new(0)),
            midi: None,
            midi_spare,
            midi_ports: Vec::new(),
            midi_selected: None,
            midi_error: None,
            silenced: false,
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

    /// The prompt's Save all: saves every file with unsaved edits, then runs the held
    /// action only if all of them saved. Otherwise the action is dropped and the results
    /// are shown.
    pub fn resolve_pending_save_all(&mut self) {
        if let Some(action) = self.pending.take()
            && self.save_all()
        {
            self.perform(action);
        }
    }

    /// Save: the selected file goes straight to its remembered destination, or the dialog
    /// opens when it has none. The results are shown only if the save failed.
    pub fn save(&mut self) {
        let Some(i) = self.selected else { return };
        let Some(FileEntry::Loaded(s)) = self.files.get(i) else {
            return;
        };
        match s.destination.clone() {
            Some(destination) => {
                let results = self.save_without_dialog(i, destination);
                if results.iter().any(|(_, r)| r.is_err()) {
                    self.save_results = Some(results);
                }
            }
            None => self.save_as(),
        }
    }

    /// Save as…: opens the dialog for the selected file.
    pub fn save_as(&mut self) {
        if let Some(i) = self.selected {
            self.ui
                .save_dialog
                .open_for(i, &self.files, !self.single_file);
        }
    }

    /// Saves every file with unsaved edits to its remembered destination, or to the default
    /// folder next to it. Returns true when all of them saved; otherwise the results are
    /// shown.
    pub fn save_all(&mut self) -> bool {
        let mut results = Vec::new();
        for i in 0..self.files.len() {
            let FileEntry::Loaded(s) = &self.files[i] else {
                continue;
            };
            if !s.history.is_dirty() {
                continue;
            }
            let destination = s
                .destination
                .clone()
                .unwrap_or_else(|| Destination::Folder(default_folder(&s.doc.path)));
            results.extend(self.save_without_dialog(i, destination));
        }
        let ok = results.iter().all(|(_, r)| r.is_ok());
        if !ok {
            self.save_results = Some(results);
        }
        ok
    }

    /// Saves file `i` to `destination` with nobody to ask: a folder save that would replace
    /// a different file of the same name is refused instead (Save as… can confirm it).
    fn save_without_dialog(&mut self, i: usize, destination: Destination) -> Vec<SaveResult> {
        if let (Some(FileEntry::Loaded(s)), Destination::Folder(dir)) =
            (self.files.get(i), &destination)
            && other_file_in_the_way(dir, s).is_some()
        {
            let message = format!(
                "a different file with this name already exists in {}; use Save as…",
                dir.display()
            );
            return vec![(s.doc.path.clone(), Err(message))];
        }
        let req = SaveRequest {
            indices: vec![i],
            destination,
            trim_after_loop: None,
        };
        perform_save(self, req)
    }

    fn run_save_command(&mut self, c: SaveCommand) {
        match c {
            SaveCommand::Save => self.save(),
            SaveCommand::SaveAs => self.save_as(),
        }
    }

    /// True while a modal (unsaved prompt, save dialog, save results) is up.
    fn modal_open(&self) -> bool {
        self.pending.is_some() || self.ui.save_dialog.open || self.save_results.is_some()
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

    /// Makes `i` the edited file and hands its sound to the audio engine. Notes still sounding
    /// from the previous file are released.
    pub fn select(&mut self, i: usize) {
        self.selected = Some(i);
        self.release_all();
        let loaded = matches!(self.files.get(i), Some(FileEntry::Loaded(_)));
        match (self.files.get(i), &mut self.engine) {
            (Some(FileEntry::Loaded(s)), Some(engine)) => engine.set_buffer(s.play_buffer()),
            (_, Some(engine)) => engine.silence(),
            _ => {}
        }
        self.silenced = !loaded;
    }

    /// The selected file, if it is loaded.
    fn selected_session(&self) -> Option<&Session> {
        match self.selected.and_then(|i| self.files.get(i)) {
            Some(FileEntry::Loaded(s)) => Some(s),
            _ => None,
        }
    }

    fn send(&mut self, c: Command) {
        if let Some(engine) = &mut self.engine {
            engine.send(c);
        }
    }

    /// Releases every note and forgets the held keys.
    fn release_all(&mut self) {
        self.send(Command::AllOff);
        self.ui.held.clear();
    }

    /// Starts `note` from the selected file's Start (the MIDI start frame).
    fn note_on(&mut self, note: u8) {
        self.ui.held.insert(note);
        let from = self.start_frame.load(Ordering::Relaxed);
        self.send(Command::NoteOn { note, from });
    }

    fn note_off(&mut self, note: u8) {
        if self.ui.held.remove(&note) {
            self.send(Command::NoteOff { note });
        }
    }

    /// Plays the root note from the start (`from_loop == false`) or from the loop start.
    fn play(&mut self, from_loop: bool) {
        let Some(e) = self.selected_session().map(|s| s.shown_edit().clone()) else {
            return;
        };
        let from = if from_loop { e.loop_start } else { e.start };
        self.send(Command::NoteOn { note: e.root, from });
    }

    /// Keeps the engine and the MIDI start frame in step with the selection each frame.
    fn sync_audio(&mut self) {
        let start = self.selected_session().map(|s| s.shown_edit().start);
        self.start_frame
            .store(start.unwrap_or(0), Ordering::Relaxed);
        if start.is_none() && !self.silenced {
            self.release_all();
            if let Some(engine) = &mut self.engine {
                engine.silence();
            }
            self.silenced = true;
        }
    }

    /// QWERTY notes and octave keys. Presses need audition on and no widget wanting the
    /// keyboard; releases always count, so a note never sticks.
    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        let accepting = self.ui.audition && !self.modal_open() && !ctx.egui_wants_keyboard_input();
        for event in events {
            match event {
                Event::WindowFocused(false) => self.release_all(),
                Event::Key {
                    key,
                    pressed: false,
                    ..
                } => {
                    if let Some(note) = qwerty_note(key, self.ui.octave_base) {
                        self.note_off(note);
                    }
                }
                Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } if accepting && !modifiers.command && !modifiers.alt => {
                    if let Some(shift) = octave_shift(key) {
                        let base = i16::from(self.ui.octave_base) + i16::from(shift);
                        self.ui.octave_base = base.clamp(12, 96) as u8;
                        // Held keys would release as different notes under the new base.
                        self.release_all();
                    } else if let Some(note) = qwerty_note(key, self.ui.octave_base) {
                        self.note_on(note);
                    }
                }
                _ => {}
            }
        }
    }

    /// Switches the MIDI input to port `index`, or off for `None`.
    fn choose_midi_port(&mut self, index: Option<usize>) {
        self.send(Command::AllOff);
        let producer = match self.midi.take() {
            Some(conn) => Some(conn.close()),
            None => self.midi_spare.take(),
        };
        self.midi_selected = None;
        self.midi_error = None;
        let Some(index) = index else {
            self.midi_spare = producer;
            return;
        };
        let Some(tx) = producer else { return };
        match connect_midi(index, tx, Arc::clone(&self.start_frame)) {
            Ok(conn) => {
                self.midi = Some(conn);
                self.midi_selected = Some(index);
            }
            Err((message, tx)) => {
                self.midi_spare = Some(tx);
                self.midi_error = Some(message);
            }
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
            // The first loaded file is selected; a file selected while it was still loading
            // gets its sound now.
            let loaded = matches!(self.files[i], FileEntry::Loaded(_));
            if loaded && self.selected.is_none_or(|s| s == i) {
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
        if self.modal_open() {
            return;
        }
        // The shift variants first: Ctrl+Shift+S would otherwise also match plain Ctrl+S.
        let (save_as, save) = ctx.input_mut(|i| {
            let save_as = i.consume_shortcut(&SAVE_AS_SHORTCUT);
            (save_as, i.consume_shortcut(&SAVE_SHORTCUT))
        });
        if save_as {
            self.save_as();
        } else if save {
            self.save();
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
        self.sync_audio();
        self.handle_keyboard(&ctx);

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
        self.inspector_panel(ui);
        self.keyboard_panel(ui);
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));
        self.unsaved_prompt(&ctx);
        if let Some(req) = save_dialog(&ctx, &mut self.ui.save_dialog, &self.files) {
            self.save_results = Some(perform_save(self, req));
        }
        if let Some(results) = &self.save_results
            && save_results_window(&ctx, results)
        {
            self.save_results = None;
        }
        self.flush_preview(&ctx);
    }

    /// Publishes the selected file's held-back drag preview, or asks for a repaint when
    /// its wait is over.
    fn flush_preview(&mut self, ctx: &egui::Context) {
        let engine = self.engine.as_ref();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i))
            && let Some(wait) = s.flush_preview(engine)
        {
            ctx.request_repaint_after(wait);
        }
    }

    fn inspector_panel(&mut self, ui: &mut egui::Ui) {
        let engine = self.engine.as_ref();
        let ui_state = &self.ui;
        let selected = self.selected.and_then(|i| self.files.get_mut(i));
        let mut command = None;
        egui::Panel::right("inspector")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=480.0)
            .show(ui, |ui| {
                if let Some(FileEntry::Loaded(s)) = selected {
                    command = inspector(ui, s, ui_state, engine);
                }
            });
        if let Some(c) = command {
            self.run_save_command(c);
        }
    }

    fn keyboard_panel(&mut self, ui: &mut egui::Ui) {
        let root = self.selected_session().map_or(60, |s| s.shown_edit().root);
        let live = self.engine.is_some() && self.ui.audition;
        let mut key = None;
        egui::Panel::bottom("keyboard")
            .exact_size(84.0)
            .show(ui, |ui| {
                ui.weak(format!(
                    "Z row starts at {}; - and = shift the octave; root key {}",
                    note_name(self.ui.octave_base),
                    note_name(root)
                ));
                ui.add_enabled_ui(live, |ui| {
                    key = keyboard_strip(ui, root, &self.ui.held);
                });
            });
        match key {
            Some((note, true)) => self.note_on(note),
            Some((note, false)) => self.note_off(note),
            None => {}
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        let mut undo = false;
        let mut redo = false;
        let mut play = None;
        let mut stop = false;
        let mut midi_choice = None;
        let mut save = None;
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
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
                ui.add_enabled_ui(self.selected_session().is_some(), |ui| {
                    let ctx = ui.ctx().clone();
                    let hint = format!(
                        "Save to the last destination, or choose one ({})",
                        ctx.format_shortcut(&SAVE_SHORTCUT)
                    );
                    if ui.button("Save").on_hover_text(hint).clicked() {
                        save = Some(SaveCommand::Save);
                    }
                    let hint = format!(
                        "Choose where and how to save ({})",
                        ctx.format_shortcut(&SAVE_AS_SHORTCUT)
                    );
                    if ui.button("Save as…").on_hover_text(hint).clicked() {
                        save = Some(SaveCommand::SaveAs);
                    }
                });
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
                let can_play = self.engine.is_some() && self.selected_session().is_some();
                ui.add_enabled_ui(can_play, |ui| {
                    if ui
                        .button("Play")
                        .on_hover_text("Play from Start through the loop at the root key")
                        .clicked()
                    {
                        play = Some(false);
                    }
                    if ui
                        .button("Play loop")
                        .on_hover_text("Play from the loop start at the root key")
                        .clicked()
                    {
                        play = Some(true);
                    }
                    stop = ui.button("Stop").clicked();
                });
                ui.add_enabled_ui(self.engine.is_some(), |ui| {
                    let on = ui
                        .toggle_value(&mut self.ui.audition, "Audition")
                        .on_hover_text("Play notes from the computer keyboard (Z and Q rows)");
                    if on.changed() && !self.ui.audition {
                        stop = true;
                    }
                    self.midi_menu(ui, &mut midi_choice);
                });
                ui.separator();
                self.status(ui);
            });
        });
        if let Some(from_loop) = play {
            self.play(from_loop);
        }
        if stop {
            self.release_all();
        }
        if let Some(choice) = midi_choice {
            self.choose_midi_port(choice);
        }
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        if let Some(c) = save {
            self.run_save_command(c);
        }
        if let Some(a) = action {
            self.request(a);
        }
    }

    /// The MIDI input menu. Ports are listed afresh each time it is opened.
    fn midi_menu(&mut self, ui: &mut egui::Ui, choice: &mut Option<Option<usize>>) {
        let can_connect = self.midi.is_some() || self.midi_spare.is_some();
        ui.add_enabled_ui(can_connect, |ui| {
            let menu = ui.menu_button("MIDI", |ui| {
                if ui
                    .selectable_label(self.midi_selected.is_none(), "None")
                    .clicked()
                {
                    *choice = Some(None);
                }
                for (i, name) in self.midi_ports.iter().enumerate() {
                    if ui
                        .selectable_label(self.midi_selected == Some(i), name)
                        .clicked()
                    {
                        *choice = Some(Some(i));
                    }
                }
                if self.midi_ports.is_empty() {
                    ui.weak("No MIDI input ports found");
                }
                if let Some(err) = &self.midi_error {
                    ui.colored_label(colors::END, err);
                }
            });
            if menu.response.clicked() {
                self.midi_ports = list_midi_ports();
            }
        });
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
        if let Some(err) = &self.midi_error {
            ui.colored_label(colors::END, err);
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
                egui::Panel::bottom("seam")
                    .resizable(true)
                    .default_size(SEAM_PANEL_HEIGHT)
                    .size_range(120.0..=600.0)
                    .show(ui, |ui| seam_view(ui, s, ui_state, engine));
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
        let mut save_all = false;
        egui::Modal::new(egui::Id::unique("unsaved_changes")).show(ctx, |ui| {
            ui.heading("Unsaved changes");
            ui.label(format!(
                "{count} file(s) have edits that have not been saved. Discarding them cannot be undone."
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                save_all = ui
                    .button("Save all")
                    .on_hover_text("Save each file to its last destination or the default folder")
                    .clicked();
                if ui.button("Discard").clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if save_all {
            self.resolve_pending_save_all();
        } else if let Some(discard) = choice {
            self.resolve_pending(discard);
        }
    }
}

impl eframe::App for LimewaveApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use limewave_core::document::load_wav_bytes;
    use limewave_core::edit::LoopMode;
    use limewave_core::testwav::{sine, wav_bytes};
    use limewave_core::wav::SampleFormat;

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
    fn a_file_selected_while_loading_gets_its_sound_once_loaded() {
        let mut app = LimewaveApp::for_test();
        app.files = vec![
            FileEntry::Loading(PathBuf::from("a.wav")),
            FileEntry::Loading(PathBuf::from("t.wav")),
        ];
        let (tx, rx) = std::sync::mpsc::channel();
        app.loader = Some(rx);
        app.select(1);
        assert!(app.silenced, "nothing to play yet");

        tx.send((1, Ok(session().doc))).unwrap();
        app.drain_loader();
        assert_eq!(app.selected, Some(1));
        assert!(!app.silenced, "the loaded file's sound was handed over");

        // A file that finishes loading while another is selected changes nothing.
        tx.send((0, Ok(session().doc))).unwrap();
        app.drain_loader();
        assert_eq!(app.selected, Some(1));
    }

    #[test]
    fn a_throttled_preview_is_published_when_the_drag_holds_still() {
        let mut s = session();
        let looped = |crossfade| EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 2000,
            loop_end: 6000,
            crossfade,
            ..s.edit().clone()
        };
        let (faded, plain) = (looped(10), looped(0));
        s.set_preview(faded, None);
        assert_eq!(s.flush_preview(None), None, "published at once");
        s.set_preview(plain, None);
        assert_ne!(s.rendered, s.doc.audio, "throttled");

        // No further drag events: the app asks to be woken when the wait is over.
        let wait = s.flush_preview(None).expect("a repaint is needed");
        assert!(wait > Duration::ZERO && wait <= PREVIEW_PUSH_INTERVAL);
        assert_ne!(s.rendered, s.doc.audio, "still waiting");

        s.last_preview_push = Instant::now().checked_sub(PREVIEW_PUSH_INTERVAL);
        assert_eq!(s.flush_preview(None), None);
        assert_eq!(s.rendered, s.doc.audio, "the held preview is published");
        assert_eq!(s.flush_preview(None), None, "nothing left to publish");
    }

    #[test]
    fn dirty_sessions_hold_back_open_until_discard() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = LimewaveApp::for_test();
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
