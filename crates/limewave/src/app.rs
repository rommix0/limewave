//! The application: sessions, folder loading, panels and the unsaved-changes prompt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, KeyboardShortcut, Modifiers};
use limewave_core::document::{WavDocument, load_wav};
use limewave_core::dsp::{PeakPyramid, mono_sum};
use limewave_core::edit::{
    EditState, History, Marker, crop_to_loop, set_loop_to_selection, set_trim_to_selection,
    trim_after_loop,
};
use limewave_core::effects::{
    AppliedEffect, Effect, Op, apply_effect, mirror_edit, peak, plan_effect, process,
    silence_bounds,
};
use limewave_core::notes::note_name;
use limewave_core::render::render;
use limewave_core::save::{Destination, default_folder};
use limewave_core::wav::SampleFormat;

use crate::audio::engine::{AudioEngine, Command, Cursor};
use crate::audio::keys::{octave_shift, qwerty_note};
use crate::audio::midi::{MidiConnection, connect_midi, list_midi_ports};
use crate::audio::voice::{PlayBuffer, play_buffer};
use crate::folder;
use crate::theme::{self, colors};
use crate::ui::coords::ViewTransform;
use crate::ui::effects::{EffectDialog, EffectParams, edit_menu, effect_dialog, effects_menu};
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
pub(crate) const SELECT_ALL_SHORTCUT: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND, Key::A);

/// Default height of the seam panel: its controls plus a plot of about 200 px.
const SEAM_PANEL_HEIGHT: f32 = 300.0;

type LoadResult = (usize, Result<WavDocument, String>);

/// The next [`Session::source`]. Starts at 1: source 0 is the engine's empty buffer.
static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);

/// One open file: the decoded document, its edit history and the data derived from it.
pub struct Session {
    pub doc: WavDocument,
    pub history: History,
    /// `doc.audio` after the current edit's effects: what the views draw and the engine plays
    /// (apart from the crossfade). Same frame count as `doc.audio`, but the channel count may
    /// differ after a channel tool.
    pub processed: Vec<Vec<f32>>,
    /// The effects `processed` was made with.
    processed_for: Vec<AppliedEffect>,
    /// One peak pyramid per channel of `processed`.
    pub peaks: Vec<PeakPyramid>,
    /// Channel average of `processed`, for analysis and drawing.
    pub mono: Vec<f32>,
    /// `processed` with the current crossfade applied.
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
    /// Identifies this session's play buffers, so voices released on another file finish on
    /// that file's audio. Unique per session: one rebased after a save gets a new id.
    pub source: u64,
}

impl Session {
    pub fn new(doc: WavDocument) -> Session {
        let initial = doc.initial_edit();
        let processed = process(&doc.audio, &initial.effects);
        let peaks = processed.iter().map(|c| PeakPyramid::build(c)).collect();
        let mono = mono_sum(&processed);
        let rendered = render(&processed, &initial);
        Session {
            doc,
            processed_for: initial.effects.clone(),
            processed,
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
            source: NEXT_SOURCE.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn edit(&self) -> &EditState {
        self.history.current()
    }

    /// True when the processed audio peaks above full scale between Start and End and the
    /// file is an integer format, so saving would clip.
    pub fn will_clip(&self) -> bool {
        let e = self.edit();
        matches!(self.doc.format.format, SampleFormat::Int { .. })
            && peak(&self.processed, e.start.min(e.end), e.start.max(e.end)) > 1.0
    }

    /// The edit the views draw: the drag preview if there is one, else the current edit.
    pub fn shown_edit(&self) -> &EditState {
        self.preview.as_ref().unwrap_or_else(|| self.edit())
    }

    /// The buffer the audio engine plays for the current edit.
    pub fn play_buffer(&self) -> PlayBuffer {
        play_buffer(
            &self.processed,
            self.doc.format.sample_rate,
            self.edit(),
            self.source,
        )
    }

    /// Records `e` (clamped to the file) as the new current edit.
    pub fn apply(&mut self, e: EditState, engine: Option<&AudioEngine>) {
        self.just_saved = false;
        self.preview = None;
        self.history.push(e.clamped(self.doc.frames));
        self.refresh(engine);
    }

    /// Applies `effect` as one history entry: planned on the processed audio, to `selection`
    /// (inclusive frames) or to the range the effect defaults to. A Reverse that works on
    /// Start...End mirrors the loop points with it. Returns false, changing nothing, when
    /// the effect has nothing to do.
    pub fn apply_effect(
        &mut self,
        effect: Effect,
        selection: Option<(usize, usize)>,
        engine: Option<&AudioEngine>,
    ) -> bool {
        let e = self.edit();
        let Some(a) = plan_effect(effect, &self.processed, e, selection) else {
            return false;
        };
        let mut next = match a.op {
            Op::Reverse {
                mirror_markers: true,
            } => mirror_edit(e, a.first, a.last),
            _ => e.clone(),
        };
        next.effects.push(a);
        self.apply(next, engine);
        true
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

    /// Brings `processed`, `peaks` and `mono` up to date with the current edit's effects:
    /// only the new effects are applied when they extend the ones already applied, else the
    /// audio is processed afresh. Does nothing when the effects are unchanged.
    fn sync_processed(&mut self) {
        let effects = &self.history.current().effects;
        if *effects == self.processed_for {
            return;
        }
        if effects.starts_with(&self.processed_for) {
            for a in &effects[self.processed_for.len()..] {
                apply_effect(&mut self.processed, a);
            }
        } else {
            self.processed = process(&self.doc.audio, effects);
        }
        self.processed_for.clone_from(effects);
        self.peaks = self
            .processed
            .iter()
            .map(|c| PeakPyramid::build(c))
            .collect();
        self.mono = mono_sum(&self.processed);
    }

    /// Recomputes `processed` and `rendered` for the current edit and publishes it to the
    /// engine.
    fn refresh(&mut self, engine: Option<&AudioEngine>) {
        self.sync_processed();
        let e = self.history.current().clone();
        self.publish(&e, engine);
    }

    /// Renders `e` into `rendered` and, with an engine, publishes the matching play buffer.
    fn publish(&mut self, e: &EditState, engine: Option<&AudioEngine>) {
        match engine {
            Some(engine) => {
                let buffer =
                    play_buffer(&self.processed, self.doc.format.sample_rate, e, self.source);
                self.rendered.clone_from(&buffer.audio);
                engine.set_buffer(buffer);
            }
            None => self.rendered = render(&self.processed, e),
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
    /// The marker whose exact position is being typed, and the text typed so far.
    pub marker_entry: Option<(Marker, String)>,
    /// The selected frames of the shown file (inclusive), from dragging on the waveform.
    pub selection: Option<(usize, usize)>,
    /// Where Play starts instead of Start, set by clicking on the waveform.
    pub play_from: Option<usize>,
    /// The remembered values of the Effects dialogs.
    pub effect_params: EffectParams,
    /// The Effects dialog that is open.
    pub effect_dialog: Option<EffectDialog>,
    /// Why the last action did nothing, shown in the toolbar until the next action.
    pub message: Option<String>,
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
            marker_entry: None,
            selection: None,
            play_from: None,
            effect_params: EffectParams::default(),
            effect_dialog: None,
            message: None,
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
    /// Whether a widget held the keyboard focus at the end of the last frame. egui drops the
    /// focus when Escape arrives, before this app sees the key, so the field that Escape
    /// closes would otherwise look unfocused.
    focus_last_frame: bool,
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
            focus_last_frame: false,
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

    /// True while a modal (unsaved prompt, save dialog, save results, effect dialog) is up.
    fn modal_open(&self) -> bool {
        self.pending.is_some()
            || self.ui.save_dialog.open
            || self.save_results.is_some()
            || self.ui.effect_dialog.is_some()
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
        self.ui.marker_entry = None;
        self.ui.selection = None;
        self.ui.play_from = None;
        self.ui.message = None;
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
    pub(crate) fn selected_session(&self) -> Option<&Session> {
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

    /// Where Play starts: the loop start for Play loop, otherwise the play-from point or
    /// Start. `None` without a loaded file.
    pub fn play_start(&self, from_loop: bool) -> Option<usize> {
        let e = self.selected_session()?.shown_edit();
        Some(if from_loop {
            e.loop_start
        } else {
            self.ui.play_from.unwrap_or(e.start)
        })
    }

    /// Plays the root note from [`LimewaveApp::play_start`].
    fn play(&mut self, from_loop: bool) {
        let (Some(from), Some(note)) = (
            self.play_start(from_loop),
            self.selected_session().map(|s| s.shown_edit().root),
        ) else {
            return;
        };
        self.send(Command::NoteOn { note, from });
    }

    /// Selects Start...End.
    pub fn select_all(&mut self) {
        self.ui.message = None;
        if let Some(e) = self.selected_session().map(|s| s.shown_edit()) {
            self.ui.selection = Some((e.start, e.end));
        }
    }

    /// Clears the selection and the play-from point.
    pub fn clear_selection(&mut self) {
        self.ui.message = None;
        self.ui.selection = None;
        self.ui.play_from = None;
    }

    /// Plays the selection once at the root key, releasing at its end.
    pub fn play_selection(&mut self) {
        self.ui.message = None;
        let (Some((first, last)), Some(note)) = (
            self.ui.selection,
            self.selected_session().map(|s| s.shown_edit().root),
        ) else {
            return;
        };
        // The selection is inclusive: the voice stops on the frame after its last one.
        self.send(Command::PlayRange {
            note,
            from: first,
            to: last + 1,
        });
    }

    /// Records the edit `f` makes of the selected file's current edit as one history entry,
    /// or shows why it refused. An edit that changes nothing adds no entry.
    fn edit_selected(&mut self, f: impl FnOnce(&Session) -> Result<EditState, String>) {
        self.ui.message = None;
        let engine = self.engine.as_ref();
        let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i)) else {
            return;
        };
        match f(s) {
            Ok(e) if e != *s.edit() => s.apply(e, engine),
            Ok(_) => {}
            Err(message) => self.ui.message = Some(message),
        }
    }

    /// Makes the selection the loop.
    pub fn set_loop_to_selection(&mut self) {
        let Some(sel) = self.ui.selection else { return };
        self.edit_selected(|s| Ok(set_loop_to_selection(s.edit(), sel, s.doc.frames)));
    }

    /// Makes the selection the trim; refused when the loop would fall outside it.
    pub fn set_trim_to_selection(&mut self) {
        let Some(sel) = self.ui.selection else { return };
        self.edit_selected(|s| set_trim_to_selection(s.edit(), sel));
    }

    /// Moves End to the loop end plus the save dialog's padding.
    pub fn trim_after_loop(&mut self) {
        let padding = self.ui.save_dialog.padding;
        self.edit_selected(|s| Ok(trim_after_loop(s.edit(), padding, s.doc.frames)));
    }

    /// Moves Start to the loop start and End to the loop end plus the padding.
    pub fn crop_to_loop(&mut self) {
        let padding = self.ui.save_dialog.padding;
        self.edit_selected(|s| Ok(crop_to_loop(s.edit(), padding, s.doc.frames)));
    }

    /// Moves Start and End in to the first and last frame at or above `threshold_db` (kept
    /// outside the loop while it is on).
    pub fn auto_trim_silence(&mut self, threshold_db: f32) {
        self.edit_selected(|s| {
            let e = s.edit();
            let (start, end) = silence_bounds(&s.processed, threshold_db, e)
                .ok_or("Everything is below the threshold; nothing to keep")?;
            Ok(EditState {
                start,
                end,
                ..e.clone()
            }
            .clamped(s.doc.frames))
        });
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

    /// Applies `effect` to the selected file as one undoable step, to the selection (inclusive
    /// frames) if there is one. Returns false when nothing was applied.
    pub fn apply_effect(&mut self, effect: Effect) -> bool {
        self.ui.message = None;
        let selection = self.ui.selection;
        let engine = self.engine.as_ref();
        match self.selected.and_then(|i| self.files.get_mut(i)) {
            Some(FileEntry::Loaded(s)) => s.apply_effect(effect, selection, engine),
            _ => false,
        }
    }

    /// Whether `effect` would do anything to the selected file (a channel tool needs the
    /// right channel count, a Normalize needs sound).
    pub fn can_apply(&self, effect: Effect) -> bool {
        self.selected_session().is_some_and(|s| {
            plan_effect(effect, &s.processed, s.edit(), self.ui.selection).is_some()
        })
    }

    pub fn undo(&mut self) {
        self.ui.message = None;
        let engine = self.engine.as_ref();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i)) {
            s.undo(engine);
        }
    }

    pub fn redo(&mut self) {
        self.ui.message = None;
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
        // Text fields keep Ctrl+A and Escape for themselves.
        if self.focus_last_frame || ctx.egui_wants_keyboard_input() {
            return;
        }
        let (select_all, clear) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&SELECT_ALL_SHORTCUT),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if select_all {
            self.select_all();
        } else if clear {
            self.clear_selection();
        }
    }

    /// The playback cursors of the sounding voices; empty without an audio engine.
    pub fn cursors(&self) -> Vec<Cursor> {
        self.engine
            .as_ref()
            .map_or_else(Vec::new, AudioEngine::cursors)
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
        let cursors = self.cursors();
        if !cursors.is_empty() {
            ctx.request_repaint();
        }
        egui::CentralPanel::default().show(ui, |ui| self.central(ui, &cursors));
        self.unsaved_prompt(&ctx);
        effect_dialog(&ctx, self);
        if let Some(req) = save_dialog(&ctx, &mut self.ui.save_dialog, &self.files) {
            self.save_results = Some(perform_save(self, req));
        }
        if let Some(results) = &self.save_results
            && save_results_window(&ctx, results)
        {
            self.save_results = None;
        }
        self.flush_preview(&ctx);
        self.focus_last_frame = ctx.egui_wants_keyboard_input();
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
                edit_menu(ui, self);
                effects_menu(ui, self);
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
        if let Some(message) = &self.ui.message {
            ui.colored_label(colors::LOOP, message);
        }
        if !self.single_file && !self.files.is_empty() {
            ui.weak(format!("{} files", self.files.len()));
        }
    }

    fn central(&mut self, ui: &mut egui::Ui, cursors: &[Cursor]) {
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
                overview(ui, s, cursors);
                ui.add_space(4.0);
                egui::Panel::bottom("seam")
                    .resizable(true)
                    .default_size(SEAM_PANEL_HEIGHT)
                    .size_range(120.0..=600.0)
                    .show(ui, |ui| seam_view(ui, s, ui_state, engine, cursors));
                let size = ui.available_size();
                ui.allocate_ui(size, |ui| waveform_view(ui, s, ui_state, engine, cursors));
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
    use limewave_core::effects::FadeCurve;
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

    fn stereo_session() -> Session {
        let bytes = wav_bytes(
            &[
                sine(220.0, 44100, 8000, 0.25),
                sine(330.0, 44100, 8000, 0.125),
            ],
            44100,
            SampleFormat::Int { bits: 16 },
            Vec::new(),
        );
        Session::new(load_wav_bytes(PathBuf::from("st.wav"), &bytes).unwrap())
    }

    const GAIN_6: Effect = Effect::Gain { db: 6.0206 };

    #[test]
    fn effect_updates_processed_peaks_and_undo_restores() {
        let mut s = session();
        assert!(s.apply_effect(GAIN_6, None, None));
        assert!(s.history.is_dirty());
        for (p, d) in s.processed[0].iter().zip(&s.doc.audio[0]) {
            assert!((p - 2.0 * d).abs() < 1e-3);
        }
        let (lo, hi) = s.peaks[0].min_max(&s.processed[0], 0, s.doc.frames);
        let (lo0, hi0) = PeakPyramid::build(&s.doc.audio[0]).min_max(&s.doc.audio[0], 0, 8000);
        assert!((hi - 2.0 * hi0).abs() < 1e-3 && (lo - 2.0 * lo0).abs() < 1e-3);
        assert_eq!(s.mono, mono_sum(&s.processed));
        assert_eq!(s.rendered, s.processed, "no crossfade: rendered follows");

        s.undo(None);
        assert_eq!(s.processed, s.doc.audio);
        assert_eq!(s.mono, mono_sum(&s.doc.audio));
        assert!(!s.history.is_dirty());
    }

    #[test]
    fn nothing_to_do_adds_no_history_entry() {
        let mut s = session();
        // Mono-to-stereo on a stereo file does not apply; neither does stereo-to-mono on mono.
        assert!(!s.apply_effect(Effect::StereoToMono, None, None));
        assert!(!s.history.is_dirty());
    }

    #[test]
    fn reverse_whole_and_undo_restore_markers_crossfade_and_audio() {
        let mut s = session();
        let e = EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 2000,
            loop_end: 6000,
            crossfade: 10,
            ..s.edit().clone()
        };
        s.apply(e, None);
        assert!(s.apply_effect(Effect::Reverse, None, None));
        let r = s.edit();
        assert_eq!((r.loop_start, r.loop_end, r.crossfade), (1999, 5999, 10));
        let mut want = s.doc.audio[0].clone();
        want.reverse();
        assert_eq!(s.processed[0], want);

        s.undo(None);
        let r = s.edit();
        assert_eq!((r.loop_start, r.loop_end, r.crossfade), (2000, 6000, 10));
        assert_eq!(s.processed, s.doc.audio);
    }

    #[test]
    fn incremental_processing_matches_full_replay() {
        let mut s = session();
        assert!(s.apply_effect(GAIN_6, Some((100, 4000)), None));
        assert!(s.apply_effect(Effect::FadeOut(FadeCurve::Linear), None, None));
        assert!(s.apply_effect(Effect::Invert, Some((10, 20)), None));
        let full = |s: &Session| process(&s.doc.audio, &s.edit().effects);
        assert_eq!(s.edit().effects.len(), 3);
        assert_eq!(s.processed, full(&s));
        assert_eq!(s.processed_for, s.edit().effects);

        s.undo(None);
        assert_eq!(s.processed, full(&s));
        s.redo(None);
        assert_eq!(s.processed, full(&s));
        assert_eq!(s.mono, mono_sum(&s.processed));

        // A different effect after an undo is not an extension: the cache is rebuilt.
        s.undo(None);
        s.undo(None);
        assert!(s.apply_effect(Effect::Invert, None, None));
        assert_eq!(s.processed, full(&s));
    }

    #[test]
    fn drag_previews_never_reprocess() {
        let mut s = session();
        assert!(s.apply_effect(GAIN_6, None, None));
        // Corrupt the cache: a re-process would repair it.
        s.processed[0][0] = 9.0;
        let e = EditState {
            start: 5,
            ..s.edit().clone()
        };
        s.set_preview(e, None);
        assert_eq!(s.processed[0][0], 9.0);
    }

    #[test]
    fn channel_tool_changes_the_lane_count() {
        let mut s = stereo_session();
        assert_eq!((s.processed.len(), s.peaks.len()), (2, 2));
        assert!(s.apply_effect(Effect::StereoToMono, None, None));
        assert_eq!((s.processed.len(), s.peaks.len()), (1, 1));
        assert_eq!(s.rendered.len(), 1);
        assert_eq!(s.doc.audio.len(), 2, "the original is untouched");

        let mut app = LimewaveApp::for_test();
        app.files = vec![FileEntry::Loaded(Box::new(s))];
        app.select(0);
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            // The seam readout uses an arrow that the test font set lacks.
            .allow_missing_glyphs()
            .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
        h.step();

        let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
            unreachable!()
        };
        s.undo(None);
        assert_eq!((s.processed.len(), s.peaks.len()), (2, 2));
        h.step();
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
