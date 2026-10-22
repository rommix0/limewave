//! The application: sessions, folder loading, panels and the unsaved-changes prompt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, KeyboardShortcut, Modifiers};
use limewave_core::document::{WavDocument, load_wav, new_document};
use limewave_core::dsp::{PeakPyramid, mono_sum};
use limewave_core::edit::{
    EditState, History, Marker, crop_to_loop, crop_to_selection, redraw_samples,
    set_loop_to_selection, set_trim_to_selection, trim_after_loop,
};
use limewave_core::effects::{
    AppliedEffect, Effect, Op, apply_effect, generated_ranges, mirror_edit, peak, plan_effect,
    process, silence_bounds,
};
use limewave_core::notes::note_name;
use limewave_core::render::render;
use limewave_core::save::{Destination, default_folder};
use limewave_core::wav::SampleFormat;

use crate::audio::engine::{AudioEngine, Command, Cursor};
use crate::audio::keys::{octave_shift, qwerty_note};
use crate::audio::midi::{MidiConnection, connect_midi, decode_last_note, list_midi_ports};
use crate::audio::voice::{Adsr, PlayBuffer, play_buffer};
use crate::folder;
use crate::render_worker::{RenderKey, RenderSlot};
use crate::theme::{self, colors};
use crate::ui::coords::ViewTransform;
use crate::ui::effects::{EffectDialog, EffectParams, edit_menu, effect_dialog, effects_menu};
use crate::ui::filelist;
use crate::ui::inspector::{InspectorAction, inspector};
use crate::ui::keyboard::{KeyEvent, keyboard_strip};
use crate::ui::new_dialog::{BitDepth, NewDialogState, new_dialog};
use crate::ui::overview::overview;
use crate::ui::save_dialog::{
    SaveCommand, SaveDialogState, SaveRequest, SaveResult, other_file_in_the_way, perform_save,
    save_dialog, save_results_window,
};
use crate::ui::seam::{DEFAULT_HALF, SeamChannel, seam_view};
use crate::ui::spectrum_dialog::{SpectrumDialogState, spectrum_dialog};
use crate::ui::tail_dialog::{TailDialogState, tail_dialog};
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

/// A [`Session::predicted_peak`] question, an effect and a selection, with its answer.
type PredictedPeak = (Effect, Option<(usize, usize)>, Option<f32>);

/// One open file: the decoded document, its edit history and the data derived from it.
pub struct Session {
    pub doc: WavDocument,
    pub history: History,
    /// `doc.audio` after the current edit's effects: what the views draw and the engine plays
    /// (apart from the crossfade). The channel count may differ after a channel tool, and the
    /// frame count after a splice step ([`Session::frames`]).
    pub processed: Vec<Vec<f32>>,
    /// The effects `processed` was made with.
    processed_for: Vec<AppliedEffect>,
    /// One peak pyramid per channel of `processed`.
    pub peaks: Vec<PeakPyramid>,
    /// Channel average of `processed`, for analysis and drawing.
    pub mono: Vec<f32>,
    /// `processed` with the current crossfade applied.
    pub rendered: Vec<Vec<f32>>,
    /// The peak of `processed` over the current edit's Start...End.
    trim_peak: f32,
    /// The last [`Session::predicted_peak`] question (effect and selection) and its answer.
    predicted: Option<PredictedPeak>,
    pub destination: Option<Destination>,
    /// Set by a save that mixed a crossfade into the file (the inspector says so); cleared
    /// by the next edit, undo or redo.
    pub just_saved: bool,
    /// A new document that exists only in memory: unsaved until a save gives it a file.
    pub untitled: bool,
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
    /// The dialog preview the engine holds instead of this session's buffer, if any.
    pub dialog_preview: Option<DialogPreview>,
}

/// A dialog's result being previewed (v0.3 §7.1, §7.2): the processed audio with the dialog's
/// step applied, published under its own source so the session's audio, history and buffer
/// stay as they are.
pub struct DialogPreview {
    /// The preview buffer's source, never the session's.
    pub source: u64,
    /// The preview audio with the step's crossfade, as the engine plays it.
    pub rendered: Vec<Vec<f32>>,
}

/// A source id no buffer has used yet.
fn next_source() -> u64 {
    NEXT_SOURCE.fetch_add(1, Ordering::Relaxed)
}

impl Session {
    pub fn new(doc: WavDocument) -> Session {
        let initial = doc.initial_edit();
        let processed = process(&doc.audio, &initial.effects);
        let peaks = processed.iter().map(|c| PeakPyramid::build(c)).collect();
        let mono = mono_sum(&processed);
        let rendered = render(&processed, &initial);
        let trim_peak = trim_peak(&processed, &initial);
        Session {
            doc,
            processed_for: initial.effects.clone(),
            processed,
            history: History::new(initial),
            peaks,
            mono,
            rendered,
            trim_peak,
            predicted: None,
            destination: None,
            just_saved: false,
            untitled: false,
            view: None,
            preview: None,
            last_preview_push: None,
            preview_held: false,
            source: next_source(),
            dialog_preview: None,
        }
    }

    pub fn edit(&self) -> &EditState {
        self.history.current()
    }

    /// Whether closing now would lose something: edits since the last save, or a document
    /// that was never saved.
    pub fn unsaved(&self) -> bool {
        self.history.is_dirty() || self.untitled
    }

    /// The length of the file after the current edit's effects: what the views show and
    /// play. It differs from `doc.frames` once a splice step (a generated tail or a spectrum
    /// loop) is in the list. Core helpers still take `doc.frames`, the original count.
    pub fn frames(&self) -> usize {
        self.processed.first().map_or(0, Vec::len)
    }

    /// The frames of generated audio in the shown edit, as sorted inclusive ranges in
    /// final coordinates: the views tint them.
    pub fn tint_ranges(&self) -> Vec<(usize, usize)> {
        generated_ranges(&self.shown_edit().effects)
    }

    /// True when the processed audio peaks above full scale between Start and End and the
    /// file is an integer format, so saving would clip.
    pub fn will_clip(&self) -> bool {
        matches!(self.doc.format.format, SampleFormat::Int { .. }) && self.trim_peak > 1.0
    }

    /// The peak over Start...End once `effect` (a Gain or Normalize) is applied to `selection`,
    /// or `None` when the effect would not scale anything. The answer is kept until the
    /// question or the edit changes, so the dialog asking every frame does not rescan.
    pub fn predicted_peak(
        &mut self,
        effect: Effect,
        selection: Option<(usize, usize)>,
    ) -> Option<f32> {
        if let Some((cached_effect, cached_selection, p)) = &self.predicted
            && *cached_effect == effect
            && *cached_selection == selection
        {
            return *p;
        }
        let p = self.scan_predicted_peak(&effect, selection);
        self.predicted = Some((effect, selection, p));
        p
    }

    fn scan_predicted_peak(
        &self,
        effect: &Effect,
        selection: Option<(usize, usize)>,
    ) -> Option<f32> {
        let e = self.edit();
        let a = plan_effect(effect.clone(), &self.processed, e, selection)?;
        let Op::Scale(k) = a.op else { return None };
        let (t0, t1) = (e.start.min(e.end), e.start.max(e.end));
        let audio = &self.processed;
        let mut p = 0.0f32;
        // Start...End before, after and inside the scaled range.
        if a.first > t0 {
            p = p.max(peak(audio, t0, (a.first - 1).min(t1)));
        }
        if a.last < t1 {
            p = p.max(peak(audio, (a.last + 1).max(t0), t1));
        }
        let (lo, hi) = (a.first.max(t0), a.last.min(t1));
        if lo <= hi {
            p = p.max(k * peak(audio, lo, hi));
        }
        Some(p)
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

    /// Redraws channel `channel` from frame `first` on with `values` (the pencil), as one history
    /// entry. Returns false, changing nothing, when nothing changes.
    pub fn redraw_samples(
        &mut self,
        channel: usize,
        first: usize,
        values: &[f32],
        engine: Option<&AudioEngine>,
    ) -> bool {
        let Some(next) = redraw_samples(self.edit(), &self.processed, channel, first, values)
        else {
            return false;
        };
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

    /// Previews a dialog's `step` before it is applied: the processed audio with `step` applied,
    /// played with `edit`'s markers and loop (the edit the dialog would apply), is published
    /// under a fresh source. Voices on the session's buffer, or on an earlier preview, finish
    /// on their own audio. The history, `processed` and `rendered` are untouched.
    pub fn preview_step(
        &mut self,
        step: &AppliedEffect,
        edit: &EditState,
        engine: Option<&AudioEngine>,
    ) {
        let mut audio = self.processed.clone();
        apply_effect(&mut audio, step);
        let source = next_source();
        let rendered = match engine {
            Some(engine) => {
                let buffer = play_buffer(&audio, self.doc.format.sample_rate, edit, source);
                let rendered = buffer.audio.clone();
                engine.set_buffer(buffer);
                rendered
            }
            None => render(&audio, edit),
        };
        self.dialog_preview = Some(DialogPreview { source, rendered });
    }

    /// Ends a dialog preview: its voices are stopped (a 5 ms fade on the preview's own audio)
    /// and the session's buffer is published again. Does nothing when no preview is live.
    pub fn end_preview(&mut self, engine: Option<&mut AudioEngine>) {
        if self.dialog_preview.take().is_none() {
            return;
        }
        let e = self.shown_edit().clone();
        match engine {
            Some(engine) => {
                engine.send(Command::AllOff);
                self.publish(&e, Some(engine));
            }
            None => self.publish(&e, None),
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
        let before = self.frames();
        self.sync_processed();
        let frames = self.frames();
        if frames != before {
            // Keep the view on the file; the next draw fits it to the real width.
            self.view = self.view.map(|v| v.clamped(frames, 1.0));
        }
        let e = self.history.current().clone();
        self.trim_peak = trim_peak(&self.processed, &e);
        self.predicted = None;
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

/// The peak of `audio` over `e`'s Start...End.
fn trim_peak(audio: &[Vec<f32>], e: &EditState) -> f32 {
    peak(audio, e.start.min(e.end), e.start.max(e.end))
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
        matches!(self, FileEntry::Loaded(s) if s.unsaved())
    }
}

/// The pause after the last settings change before a dialog renders again (v0.3 §7).
const RENDER_DEBOUNCE: Duration = Duration::from_millis(200);

/// View and tool settings that outlive a single frame.
pub struct UiState {
    pub snap: bool,
    pub lock_length: bool,
    /// Dragging on a channel draws its waveform at any zoom.
    pub draw: bool,
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
    /// Set while "Set from MIDI" is armed: the note-on sequence number seen when it was armed.
    pub midi_learn: Option<u32>,
    /// The audition envelope, shared by every file and remembered between runs.
    pub envelope: Adsr,
    /// "Release plays after loop end" (v0.3 §7.3): released looped notes leave the loop and
    /// play on to End. Off by default and remembered between runs.
    pub release_after_loop: bool,
    /// Whether a MIDI input is connected; set each frame for the inspector.
    pub midi_connected: bool,
    /// The New… dialog, while it is open.
    pub new_dialog: Option<NewDialogState>,
    /// The Generate tail… dialog, while it is open.
    pub tail_dialog: Option<TailDialogState>,
    /// The Make loop from spectrum… dialog, while it is open.
    pub spectrum_dialog: Option<SpectrumDialogState>,
    /// How long the dialogs wait after the last settings change before they render again
    /// (v0.3 §7). Tests set it to zero.
    pub render_debounce: Duration,
    /// Why the last action did nothing, shown in the toolbar until the next action.
    pub message: Option<String>,
}

impl Default for UiState {
    fn default() -> Self {
        UiState {
            snap: true,
            lock_length: false,
            draw: false,
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
            midi_learn: None,
            envelope: Adsr::DEFAULT,
            release_after_loop: false,
            midi_connected: false,
            message: None,
            new_dialog: None,
            tail_dialog: None,
            spectrum_dialog: None,
            render_debounce: RENDER_DEBOUNCE,
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

/// How long the UI keeps repainting after a note-on while it waits for the voice's cursor.
const CURSOR_WAIT: Duration = Duration::from_millis(250);

/// Keeps the UI repainting while play cursors are on screen, and from a note-on until its
/// cursor first shows. The audio thread starts the voice a callback later, so the frame that
/// sends the note sees no cursor yet; without the wait the UI would sleep until the next input.
#[derive(Default)]
struct CursorWatch {
    /// When the wait for a new voice gives up.
    until: Option<Instant>,
}

impl CursorWatch {
    fn note_started(&mut self, now: Instant) {
        self.until = Some(now + CURSOR_WAIT);
    }

    /// Whether to ask for another frame, given whether any cursor shows now.
    fn keep_repainting(&mut self, sounding: bool, now: Instant) -> bool {
        if sounding {
            self.until = None;
            return true;
        }
        let waiting = self.until.is_some_and(|t| now < t);
        if !waiting {
            self.until = None;
        }
        waiting
    }
}

/// The key the audition envelope is saved under in the app settings.
const ENVELOPE_KEY: &str = "envelope";
/// The key "Release plays after loop end" is saved under, as `"1"` or `"0"`.
const RELEASE_AFTER_LOOP_KEY: &str = "release_after_loop";

/// Which numbered name [`LimewaveApp::next_name`] hands out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewName {
    /// "Untitled N.wav", for a new blank sample.
    Untitled,
    /// "Recording N.wav", for a recording.
    Recording,
}

/// Asks where to save a file called the given name; `None` when the user cancels.
pub type SavePicker = Box<dyn FnMut(&str) -> Option<PathBuf>>;

/// The system's save-file dialog, offering WAV files.
fn system_save_picker(name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("WAV audio", &["wav", "WAV"])
        .set_file_name(name)
        .save_file()
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
    /// Asks where an untitled file goes. Tests replace it.
    pub save_picker: SavePicker,
    /// The next numbers for [`NewName::Untitled`] and [`NewName::Recording`]. They never reset.
    next_untitled: u32,
    next_recording: u32,
    /// The window title last sent, so it is sent only when it changes.
    sent_title: Option<String>,
    /// The selected file's Start frame, read by the MIDI thread for each note-on.
    start_frame: Arc<AtomicUsize>,
    /// The sequence number and note of the last MIDI note-on, written by the MIDI thread (see
    /// [`decode_last_note`]).
    pub midi_last_note: Arc<AtomicU32>,
    midi: Option<MidiConnection>,
    /// The MIDI command producer while no connection holds it.
    midi_spare: Option<rtrb::Producer<Command>>,
    midi_ports: Vec<String>,
    midi_selected: Option<usize>,
    midi_error: Option<String>,
    /// The envelope last sent to the engine, so it is sent only when it changes.
    sent_envelope: Option<Adsr>,
    /// The "Release plays after loop end" setting last sent to the engine.
    sent_release_after_loop: Option<bool>,
    /// True while the engine has been silenced because no loaded file is selected.
    silenced: bool,
    /// Keeps the play cursors moving; see [`CursorWatch`].
    cursor_watch: CursorWatch,
    /// The sequence of the last MIDI note-on the cursor watch has seen.
    midi_seen: Option<u32>,
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
        let mut app = Self::build(cc.egui_ctx.clone(), engine, audio_error);
        if let Some(storage) = cc.storage {
            app.load_settings(storage);
        }
        app
    }

    /// Reads the settings remembered between runs: the envelope and "Release plays after loop
    /// end". A missing or unreadable value leaves the default.
    fn load_settings(&mut self, storage: &dyn eframe::Storage) {
        if let Some(envelope) = storage
            .get_string(ENVELOPE_KEY)
            .and_then(|s| Adsr::from_storage_string(&s))
        {
            self.ui.envelope = envelope;
        }
        self.ui.release_after_loop =
            storage.get_string(RELEASE_AFTER_LOOP_KEY).as_deref() == Some("1");
    }

    /// An app with no audio engine and no MIDI, for tests.
    pub fn for_test() -> Self {
        let mut app = Self::build(egui::Context::default(), None, None);
        app.save_picker = Box::new(|_| None);
        app
    }

    pub(crate) fn build(
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
            save_picker: Box::new(system_save_picker),
            next_untitled: 1,
            next_recording: 1,
            sent_title: None,
            start_frame: Arc::new(AtomicUsize::new(0)),
            midi_last_note: Arc::new(AtomicU32::new(0)),
            midi: None,
            midi_spare,
            midi_ports: Vec::new(),
            midi_selected: None,
            midi_error: None,
            sent_envelope: None,
            sent_release_after_loop: None,
            silenced: false,
            cursor_watch: CursorWatch::default(),
            midi_seen: None,
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

    /// The next free "Untitled N.wav" or "Recording N.wav" (no folder).
    pub fn next_name(&mut self, kind: NewName) -> PathBuf {
        let (prefix, counter) = match kind {
            NewName::Untitled => ("Untitled", &mut self.next_untitled),
            NewName::Recording => ("Recording", &mut self.next_recording),
        };
        let name = format!("{prefix} {counter}.wav");
        *counter += 1;
        PathBuf::from(name)
    }

    /// Adds a new in-memory document to the file list and selects it. Any folder still
    /// loading in the background keeps loading.
    pub fn add_untitled(&mut self, doc: WavDocument) {
        let mut session = Session::new(doc);
        session.untitled = true;
        self.files.push(FileEntry::Loaded(Box::new(session)));
        self.single_file = false;
        self.select(self.files.len() - 1);
    }

    /// Adds a silent untitled sample and turns Draw on, ready to draw on.
    pub fn create_new(
        &mut self,
        frames: usize,
        rate: u32,
        channels: usize,
        bits: BitDepth,
        root: u8,
        whole_loop: bool,
    ) {
        let name = self.next_name(NewName::Untitled);
        let audio = vec![vec![0.0; frames]; channels];
        let doc = new_document(name, audio, rate, bits.format(), root, whole_loop);
        self.add_untitled(doc);
        self.ui.draw = true;
    }

    /// Asks where the untitled file `i` goes and writes it there. On success it becomes an
    /// ordinary file that saves back to that folder. Returns false when the user cancels or
    /// the save fails (the results are then shown).
    pub fn save_untitled(&mut self, i: usize) -> bool {
        let name = self
            .files
            .get(i)
            .map(FileEntry::file_name)
            .unwrap_or_default();
        let Some(path) = (self.save_picker)(&name) else {
            return false;
        };
        let req = SaveRequest {
            indices: vec![i],
            destination: Destination::File(path.clone()),
            trim_after_loop: None,
        };
        let results = perform_save(self, req);
        if results.iter().any(|(_, r)| r.is_err()) {
            self.save_results = Some(results);
            return false;
        }
        if let Some(FileEntry::Loaded(s)) = self.files.get_mut(i) {
            s.untitled = false;
            s.destination = path
                .parent()
                .map(|dir| Destination::Folder(dir.to_path_buf()));
        }
        true
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
        if s.untitled {
            self.save_untitled(i);
            return;
        }
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
        let Some(i) = self.selected else { return };
        if matches!(self.files.get(i), Some(FileEntry::Loaded(s)) if s.untitled) {
            self.save_untitled(i);
        } else {
            self.ui
                .save_dialog
                .open_for(i, &self.files, !self.single_file);
        }
    }

    /// Saves every file with unsaved edits to its remembered destination, or to the default
    /// folder next to it; untitled files ask for a file one at a time. Returns true when all
    /// of them saved; otherwise the results are shown. A cancelled or failed untitled save
    /// stops the rest.
    pub fn save_all(&mut self) -> bool {
        let mut results = Vec::new();
        let mut stopped = false;
        for i in 0..self.files.len() {
            let FileEntry::Loaded(s) = &self.files[i] else {
                continue;
            };
            if !s.unsaved() {
                continue;
            }
            if s.untitled {
                if !self.save_untitled(i) {
                    // A failed untitled save left its result in `save_results`; a cancel
                    // left nothing. Either way the earlier results still count.
                    results.extend(self.save_results.take().unwrap_or_default());
                    stopped = true;
                    break;
                }
                continue;
            }
            let destination = s
                .destination
                .clone()
                .unwrap_or_else(|| Destination::Folder(default_folder(&s.doc.path)));
            results.extend(self.save_without_dialog(i, destination));
        }
        let failed = results.iter().any(|(_, r)| r.is_err());
        if failed {
            self.save_results = Some(results);
        }
        !failed && !stopped
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

    /// True while a modal (unsaved prompt, save dialog, save results, the New, effect, tail
    /// or spectrum-loop dialog) is up.
    fn modal_open(&self) -> bool {
        self.pending.is_some()
            || self.ui.save_dialog.open
            || self.save_results.is_some()
            || self.ui.new_dialog.is_some()
            || self.ui.effect_dialog.is_some()
            || self.ui.tail_dialog.is_some()
            || self.ui.spectrum_dialog.is_some()
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
        // The render dialogs belong to the file they were opened for.
        let tail = self.ui.tail_dialog.take().is_some();
        let spectrum = self.ui.spectrum_dialog.take().is_some();
        if tail || spectrum {
            self.end_preview();
        }
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

    /// The selected file, if it is loaded, for changing.
    pub(crate) fn selected_session_mut(&mut self) -> Option<&mut Session> {
        match self.selected.and_then(|i| self.files.get_mut(i)) {
            Some(FileEntry::Loaded(s)) => Some(s),
            _ => None,
        }
    }

    fn send(&mut self, c: Command) {
        if matches!(
            c,
            Command::NoteOn { .. } | Command::PlayRange { .. } | Command::PreviewOn { .. }
        ) {
            self.cursor_watch.note_started(Instant::now());
        }
        if let Some(engine) = &mut self.engine {
            engine.send(c);
        }
    }

    /// Releases every note and forgets the held keys, as the toolbar's Stop does. Dialog
    /// previews fade out at once.
    pub(crate) fn release_all(&mut self) {
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

    /// Cuts the audio down to the selection now (undoable). The selection and play-from point
    /// referred to the old frames, so they are cleared, and the view refits the shorter file.
    pub fn crop_to_selection(&mut self) {
        let Some(sel) = self.ui.selection else { return };
        let before = self.selected_session().map(|s| s.edit().clone());
        self.edit_selected(|s| crop_to_selection(s.edit(), sel, s.processed.len(), s.doc.frames));
        if let Some(s) = self.selected_session_mut()
            && before.as_ref() != Some(s.edit())
        {
            s.view = None;
            self.ui.selection = None;
            self.ui.play_from = None;
        }
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
        self.ui.midi_learn = None;
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
        let last_note = Arc::clone(&self.midi_last_note);
        let ctx = self.ctx.clone();
        let wake = move || ctx.request_repaint();
        match connect_midi(index, tx, Arc::clone(&self.start_frame), last_note, wake) {
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

    /// Makes `note` the root key of the selected file as one undoable step. Does nothing when
    /// it already is.
    fn set_root(&mut self, note: u8) {
        let engine = self.engine.as_ref();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i))
            && s.edit().root != note
        {
            let e = EditState {
                root: note,
                ..s.edit().clone()
            };
            s.apply(e, engine);
        }
    }

    /// Arms "Set from MIDI", or cancels it when it is already armed.
    fn toggle_midi_learn(&mut self) {
        self.ui.midi_learn = match self.ui.midi_learn {
            Some(_) => None,
            None => {
                let seen = decode_last_note(self.midi_last_note.load(Ordering::Relaxed));
                Some(seen.map_or(0, |(sequence, _)| sequence))
            }
        };
    }

    /// While "Set from MIDI" is armed, a note-on newer than the one seen when it was armed
    /// becomes the root key and disarms it. The MIDI thread wakes the UI on each note-on.
    fn update_midi_learn(&mut self) {
        self.ui.midi_connected = self.midi.is_some();
        let Some(seen) = self.ui.midi_learn else {
            return;
        };
        if let Some((sequence, note)) =
            decode_last_note(self.midi_last_note.load(Ordering::Relaxed))
            && sequence != seen
        {
            self.ui.midi_learn = None;
            self.set_root(note);
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
        self.clamp_ui_to_file();
    }

    pub fn redo(&mut self) {
        self.ui.message = None;
        let engine = self.engine.as_ref();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i)) {
            s.redo(engine);
        }
        self.clamp_ui_to_file();
    }

    /// Keeps the selection and the play-from point inside the selected file, whose length a
    /// splice step, or its undo, changes: each is cut to the file, or cleared when it lies
    /// wholly past the end. Called after undo and redo, and every frame for the edits made
    /// elsewhere (dialogs, the inspector).
    fn clamp_ui_to_file(&mut self) {
        let Some(frames) = self.selected_session().map(Session::frames) else {
            return;
        };
        self.ui.selection = self
            .ui
            .selection
            .filter(|&(first, _)| first < frames)
            .map(|(first, last)| (first, last.min(frames - 1)));
        self.ui.play_from = self.ui.play_from.filter(|&f| f < frames);
    }

    /// Records `e`, a dialog's rendered edit, as one history entry of the selected file. The
    /// selection and play-from point are kept inside the new length at once, and a file that
    /// grew is refit in the view so its new audio shows.
    pub fn apply_dialog_edit(&mut self, e: EditState) {
        self.ui.message = None;
        let engine = self.engine.as_ref();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i)) {
            let before = s.frames();
            s.apply(e, engine);
            if s.frames() > before {
                s.view = None;
            }
        }
        self.clamp_ui_to_file();
    }

    /// What a dialog render started now is made for: the selected file, its current edit and
    /// `settings_hash`, a hash of the dialog's settings. `None` without a loaded file.
    pub fn render_key(&self, settings_hash: u64) -> Option<RenderKey> {
        let file = self.selected?;
        let s = self.selected_session()?;
        Some(RenderKey {
            file,
            effects_len: s.edit().effects.len(),
            history_cursor: s.history.cursor(),
            settings_hash,
        })
    }

    /// Takes the open dialogs' finished renders, waiting for running ones when `block`. A
    /// render made for a file or edit that is no longer current is discarded.
    fn drain_renders(&mut self, block: bool) {
        if let Some(mut d) = self.ui.tail_dialog.take() {
            self.drain_render(&mut d.analysis, block);
            self.drain_render(&mut d.render, block);
            self.ui.tail_dialog = Some(d);
        }
        if let Some(mut d) = self.ui.spectrum_dialog.take() {
            self.drain_render(&mut d.analysis, block);
            self.drain_render(&mut d.render, block);
            self.ui.spectrum_dialog = Some(d);
        }
    }

    fn drain_render<T>(&self, slot: &mut RenderSlot<T>, block: bool) {
        // A settings change drops the job, so only the file and the edit can have moved on.
        let current = slot
            .job_key()
            .and_then(|k| self.render_key(k.settings_hash));
        slot.finish(current, block);
    }

    /// Test hook: blocks until the open dialogs' running renders finish and takes their
    /// results, as the frames after they finish would. Kittest tests call it between runs.
    pub fn wait_for_renders(&mut self) {
        self.drain_renders(true);
    }

    /// Previews a dialog's `step` on the selected file; see [`Session::preview_step`].
    pub fn preview_step(&mut self, step: &AppliedEffect, edit: &EditState) {
        let engine = self.engine.as_ref();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i)) {
            s.preview_step(step, edit, engine);
        }
    }

    /// Plays the live dialog preview from `from` at the root key, with the session's attack
    /// only and its release playing on past the loop ([`Command::PreviewOn`]). With
    /// `hold_loop` it holds on the loop until released; without, it plays straight through
    /// the loop to End.
    pub fn play_preview(&mut self, from: usize, hold_loop: bool) {
        if let Some(note) = self.selected_session().map(|s| s.edit().root) {
            self.send(Command::PreviewOn {
                note,
                from,
                hold_loop,
            });
        }
    }

    /// Plays the selected file's own audio once from `from` to its last frame at the root
    /// key, ignoring the loop and End, as "Play original" in the tail dialog does: the real
    /// ending is heard even when End has been moved earlier. A live dialog preview is ended
    /// first, so the note plays the session's buffer and not the preview's.
    pub fn play_original(&mut self, from: usize) {
        self.end_preview();
        if let Some(s) = self.selected_session() {
            let (note, to) = (s.edit().root, s.frames());
            self.send(Command::PlayRange { note, from, to });
        }
    }

    /// Plays the selected file's own audio from `first` to `last` (inclusive) once at the root
    /// key, as "Play original" in the spectrum-loop dialog does. A live dialog preview is ended
    /// first, as in [`LimewaveApp::play_original`].
    pub fn play_original_range(&mut self, first: usize, last: usize) {
        self.end_preview();
        if let Some(note) = self.selected_session().map(|s| s.edit().root) {
            self.send(Command::PlayRange {
                note,
                from: first,
                to: last + 1,
            });
        }
    }

    /// Releases the root key: a held dialog preview leaves its loop and plays the generated
    /// release (v0.3 §7.1), and a "Play original" note releases as usual.
    pub fn release_root(&mut self) {
        if let Some(note) = self.selected_session().map(|s| s.edit().root) {
            self.send(Command::NoteOff { note });
        }
    }

    /// Ends the selected file's dialog preview; see [`Session::end_preview`].
    pub fn end_preview(&mut self) {
        let engine = self.engine.as_mut();
        if let Some(FileEntry::Loaded(s)) = self.selected.and_then(|i| self.files.get_mut(i)) {
            s.end_preview(engine);
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

    /// Puts the selected file's name in the window title, sending it only when it changes.
    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.selected.and_then(|i| self.files.get(i)) {
            Some(entry) => format!("{} \u{2013} Limewave", entry.file_name()),
            None => "Limewave".to_owned(),
        };
        if self.sent_title.as_ref() != Some(&title) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.sent_title = Some(title);
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
        // An open menu or popup closes on Escape when it is drawn, so leave the key to it.
        let popup_open = egui::Popup::is_any_open(ctx);
        let (select_all, clear) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&SELECT_ALL_SHORTCUT),
                !popup_open && i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if select_all {
            self.select_all();
        } else if clear {
            // Escape cancels "Set from MIDI" first, then turns Draw off, and only then clears the
            // selection.
            if self.ui.midi_learn.take().is_none() {
                if self.ui.draw {
                    self.ui.draw = false;
                } else {
                    self.clear_selection();
                }
            }
        }
    }

    /// Starts the cursor watch for a MIDI note-on not seen before.
    fn watch_midi(&mut self, now: Instant) {
        let sequence = decode_last_note(self.midi_last_note.load(Ordering::Relaxed)).map(|n| n.0);
        if sequence != self.midi_seen {
            self.midi_seen = sequence;
            if sequence.is_some() {
                self.cursor_watch.note_started(now);
            }
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
        self.drain_renders(false);
        if self.loader.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        self.handle_close_request(&ctx);
        self.handle_shortcuts(&ctx);
        self.update_title(&ctx);
        self.clamp_ui_to_file();
        self.sync_audio();
        self.handle_keyboard(&ctx);
        self.update_midi_learn();

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
        self.sync_envelope();
        self.keyboard_panel(ui);
        let cursors = self.cursors();
        let now = Instant::now();
        self.watch_midi(now);
        // Without an engine no cursor ever shows, so there is nothing to wait for.
        if self.engine.is_some() && self.cursor_watch.keep_repainting(!cursors.is_empty(), now) {
            ctx.request_repaint();
        }
        egui::CentralPanel::default().show(ui, |ui| self.central(ui, &cursors));
        self.unsaved_prompt(&ctx);
        new_dialog(&ctx, self);
        effect_dialog(&ctx, self);
        tail_dialog(&ctx, self);
        spectrum_dialog(&ctx, self);
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

    /// Sends the envelope and "Release plays after loop end" to the audio engine when they
    /// differ from the ones last sent.
    fn sync_envelope(&mut self) {
        let envelope = self.ui.envelope.clamped();
        if self.sent_envelope != Some(envelope) {
            if let Some(engine) = &mut self.engine {
                engine.set_envelope(envelope);
            }
            self.sent_envelope = Some(envelope);
        }
        let past_loop = self.ui.release_after_loop;
        if self.sent_release_after_loop != Some(past_loop) {
            if let Some(engine) = &mut self.engine {
                engine.send(Command::SetReleasePastLoop(past_loop));
            }
            self.sent_release_after_loop = Some(past_loop);
        }
    }

    fn inspector_panel(&mut self, ui: &mut egui::Ui) {
        let engine = self.engine.as_ref();
        let ui_state = &mut self.ui;
        let selected = self.selected.and_then(|i| self.files.get_mut(i));
        let mut action = InspectorAction::default();
        egui::Panel::right("inspector")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=480.0)
            .show(ui, |ui| {
                if let Some(FileEntry::Loaded(s)) = selected {
                    action = inspector(ui, s, ui_state, engine);
                }
            });
        if action.toggle_midi_learn {
            self.toggle_midi_learn();
        }
        if let Some(c) = action.save {
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
                // Not disabled when silent: right-clicking a key still sets the root.
                ui.scope(|ui| {
                    if !live {
                        ui.multiply_opacity(0.5);
                    }
                    key = keyboard_strip(ui, root, &self.ui.held, live);
                });
            });
        match key {
            Some(KeyEvent::Press(note)) => self.note_on(note),
            Some(KeyEvent::Release(note)) => self.note_off(note),
            Some(KeyEvent::SetRoot(note)) => self.set_root(note),
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
                // A widget that does not fit moves whole to the next row; its text is never
                // split into the leftover width.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                if ui.button("New…").clicked() {
                    self.ui.new_dialog = Some(NewDialogState::default());
                }
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
                ui.toggle_value(&mut self.ui.draw, "✏ Draw").on_hover_text(
                    "Draw the waveform with the mouse (each stroke is one undo step)",
                );
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
                ui.label(format!("{} frames", s.frames()));
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

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(ENVELOPE_KEY, self.ui.envelope.clamped().to_storage_string());
        let past_loop = if self.ui.release_after_loop { "1" } else { "0" };
        storage.set_string(RELEASE_AFTER_LOOP_KEY, past_loop.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_worker::spawn_render;
    use limewave_core::document::load_wav_bytes;
    use limewave_core::edit::LoopMode;
    use limewave_core::effects::{FadeCurve, MonoSource, Splice, SpliceKind};
    use limewave_core::spectrum_loop::SpectrumLoopError;
    use limewave_core::tail::TailError;
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
    fn session_shows_the_acid_root_of_a_file_without_smpl() {
        let mut acid = vec![0; 24];
        acid[0] = 0x02; // root note set
        acid[4..6].copy_from_slice(&57u16.to_le_bytes());
        let bytes = wav_bytes(
            &[sine(220.0, 44100, 100, 0.5)],
            44100,
            SampleFormat::Int { bits: 16 },
            vec![limewave_core::riff::Chunk {
                id: *b"acid",
                data: acid,
            }],
        );
        let s = Session::new(load_wav_bytes(PathBuf::from("t.wav"), &bytes).unwrap());
        assert_eq!(s.edit().root, 57);
        assert!(!s.history.is_dirty());
    }

    #[test]
    fn cursor_watch_waits_for_a_new_note_then_follows_the_cursors() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut w = CursorWatch::default();
        assert!(!w.keep_repainting(false, t0), "nothing playing");
        assert!(w.keep_repainting(true, t0), "a cursor shows");

        w.note_started(t0);
        assert!(w.keep_repainting(false, ms(10)), "waiting for the voice");
        assert!(w.keep_repainting(true, ms(20)));
        // Once the cursor has shown, its end stops the repaints.
        assert!(!w.keep_repainting(false, ms(30)));

        // A note that never sounds stops the wait in time.
        w.note_started(ms(100));
        assert!(w.keep_repainting(false, ms(100) + CURSOR_WAIT / 2));
        assert!(!w.keep_repainting(false, ms(100) + CURSOR_WAIT));
    }

    #[test]
    fn notes_from_the_ui_and_from_midi_start_the_cursor_watch() {
        let mut app = LimewaveApp::for_test();
        let now = Instant::now();
        assert!(!app.cursor_watch.keep_repainting(false, now));
        app.note_on(60);
        assert!(app.cursor_watch.keep_repainting(false, now));

        let mut app = LimewaveApp::for_test();
        app.watch_midi(now);
        assert!(!app.cursor_watch.keep_repainting(false, now), "no note yet");
        app.midi_last_note.store((1 << 8) | 60, Ordering::Relaxed);
        app.watch_midi(now);
        assert!(
            app.cursor_watch.keep_repainting(false, now),
            "a new MIDI note"
        );
        app.cursor_watch = CursorWatch::default();
        app.watch_midi(now);
        assert!(
            !app.cursor_watch.keep_repainting(false, now),
            "already seen"
        );
    }

    #[test]
    fn envelope_default_and_sync() {
        assert_eq!(UiState::default().envelope, Adsr::DEFAULT);
        let mut app = LimewaveApp::for_test();
        app.ui.envelope.release_ms = 80.0;
        app.sync_envelope();
        let sent = app.sent_envelope.expect("envelope recorded");
        assert_eq!(sent.release_ms, 80.0);
        app.sync_envelope();
        assert_eq!(app.sent_envelope, Some(sent));
    }

    /// App settings held in memory, standing in for eframe's storage.
    #[derive(Default)]
    struct MemStorage(std::collections::HashMap<String, String>);

    impl eframe::Storage for MemStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_string(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn release_after_loop_setting_is_remembered() {
        use eframe::Storage as _;
        assert!(!UiState::default().release_after_loop, "off by default");
        let mut app = LimewaveApp::for_test();
        app.ui.release_after_loop = true;
        let mut storage = MemStorage::default();
        eframe::App::save(&mut app, &mut storage);
        assert_eq!(
            storage.get_string("release_after_loop").as_deref(),
            Some("1")
        );

        let mut restored = LimewaveApp::for_test();
        restored.load_settings(&storage);
        assert!(restored.ui.release_after_loop);

        let mut fresh = LimewaveApp::for_test();
        fresh.load_settings(&MemStorage::default());
        assert!(!fresh.ui.release_after_loop, "a missing key gives false");

        app.ui.release_after_loop = false;
        eframe::App::save(&mut app, &mut storage);
        assert_eq!(
            storage.get_string("release_after_loop").as_deref(),
            Some("0")
        );
        restored.load_settings(&storage);
        assert!(!restored.ui.release_after_loop);
    }

    #[test]
    fn release_after_loop_is_sent_when_it_changes() {
        let mut app = LimewaveApp::for_test();
        app.sync_envelope();
        assert_eq!(app.sent_release_after_loop, Some(false));
        app.ui.release_after_loop = true;
        app.sync_envelope();
        assert_eq!(app.sent_release_after_loop, Some(true));
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
    fn a_drag_back_to_where_it_began_renders_the_current_edit_again() {
        let mut s = session();
        let plain = EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 2000,
            loop_end: 6000,
            ..s.edit().clone()
        };
        s.apply(plain.clone(), None);
        let rendered = s.rendered.clone();
        s.set_preview(
            EditState {
                crossfade: 10,
                ..plain.clone()
            },
            None,
        );
        assert_ne!(s.rendered, rendered, "the faded preview renders");
        // Back where it began, within the throttle: the view follows, the render waits.
        s.set_preview(plain.clone(), None);
        assert_ne!(s.rendered, rendered, "throttled");

        s.commit_preview(None);
        assert!(s.preview.is_none());
        assert_eq!(s.edit(), &plain);
        assert_eq!(s.rendered, rendered, "the current edit is rendered again");
        // No history entry was added: one undo is back at the file as loaded.
        s.undo(None);
        assert!(!s.history.is_dirty());
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
        assert!(!s.apply_effect(Effect::StereoToMono(MonoSource::Mix), None, None));
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

        // A splice in the list, with effects before and after it.
        add_tail(&mut s, 7000, 1000);
        assert_eq!(s.frames(), 9000);
        assert_eq!(s.processed, full(&s));
        assert!(s.apply_effect(GAIN_6, Some((8000, 8500)), None));
        assert_eq!(s.processed, full(&s));
        assert_eq!(s.processed_for, s.edit().effects);
        s.undo(None);
        assert_eq!(s.processed, full(&s));
        s.undo(None);
        assert_eq!(s.processed, full(&s));
        assert_eq!(s.frames(), 8000);
        s.redo(None);
        s.redo(None);
        assert_eq!(s.processed, full(&s));
        assert_eq!(s.mono, mono_sum(&s.processed));
    }

    /// A 1000-frame mono sine.
    fn short_session() -> Session {
        let bytes = wav_bytes(
            &[sine(220.0, 44100, 1000, 0.5)],
            44100,
            SampleFormat::Int { bits: 16 },
            Vec::new(),
        );
        Session::new(load_wav_bytes(PathBuf::from("short.wav"), &bytes).unwrap())
    }

    /// A tail splice replacing `first` to the end of a file of `frames` frames and `channels`
    /// channels with `frames - first + added` frames, the `added` ones generated.
    fn tail(channels: usize, frames: usize, first: usize, added: usize) -> AppliedEffect {
        let kept = frames - first;
        let len = kept + added;
        let audio = vec![(0..len).map(|i| (i as f32 * 0.05).sin() * 0.25).collect(); channels];
        AppliedEffect {
            op: Op::Splice(Arc::new(Splice {
                kind: SpliceKind::Tail,
                audio,
                generated: (kept, len - 1),
            })),
            first,
            last: frames - 1,
        }
    }

    /// Applies a tail splice from `first` that adds `added` frames to `s`.
    fn add_tail(s: &mut Session, first: usize, added: usize) {
        let mut e = s.edit().clone();
        e.effects
            .push(tail(s.processed.len(), s.frames(), first, added));
        s.apply(e, None);
    }

    fn harness_for(app: LimewaveApp) -> egui_kittest::Harness<'static, LimewaveApp> {
        egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            // The seam readout uses an arrow that the test font set lacks.
            .allow_missing_glyphs()
            .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app)
    }

    #[test]
    fn frames_follows_a_splice_and_its_undo() {
        let mut s = short_session();
        assert_eq!(s.frames(), 1000);
        add_tail(&mut s, 900, 500);
        assert_eq!(s.frames(), 1500);
        assert_eq!(s.processed[0].len(), 1500);
        assert_eq!(s.rendered[0].len(), 1500);
        s.undo(None);
        assert_eq!(s.frames(), 1000);
        s.redo(None);
        assert_eq!(s.frames(), 1500);
    }

    #[test]
    fn undo_of_a_tail_clamps_view_selection_and_play_from() {
        let mut s = short_session();
        add_tail(&mut s, 900, 500);
        s.view = Some(ViewTransform {
            first: 1200.0,
            frames_per_px: 0.25,
        });
        let mut app = LimewaveApp::for_test();
        app.files = vec![FileEntry::Loaded(Box::new(s))];
        app.select(0);
        app.ui.selection = Some((1300, 1450));
        app.ui.play_from = Some(1400);
        app.undo();

        let s = app.selected_session().unwrap();
        assert_eq!(s.frames(), 1000);
        let view = s.view.expect("the view is kept");
        assert!((0.0..1000.0).contains(&view.first), "{view:?}");
        assert_eq!(app.ui.selection, None, "wholly past the end");
        assert_eq!(app.ui.play_from, None, "past the end");

        // A selection reaching past the end is cut to it.
        app.redo();
        app.ui.selection = Some((900, 1450));
        app.ui.play_from = Some(999);
        app.undo();
        assert_eq!(app.ui.selection, Some((900, 999)));
        assert_eq!(app.ui.play_from, Some(999));

        let mut h = harness_for(app);
        h.step();
        let s = h.state().selected_session().unwrap();
        let view = s.view.unwrap();
        assert!(view.first >= 0.0 && view.first < 1000.0, "{view:?}");
    }

    #[test]
    fn generated_audio_is_tinted() {
        let mut s = short_session();
        assert!(s.tint_ranges().is_empty());
        add_tail(&mut s, 900, 500);
        assert_eq!(s.tint_ranges(), vec![(1000, 1499)]);
        // A preview shows the tint of the edit it shows.
        let mut e = s.edit().clone();
        e.start = 10;
        s.set_preview(e, None);
        assert_eq!(s.tint_ranges(), vec![(1000, 1499)]);
        s.commit_preview(None);

        let mut app = LimewaveApp::for_test();
        app.files = vec![FileEntry::Loaded(Box::new(s))];
        app.select(0);
        let mut h = harness_for(app);
        h.step();
        let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
            unreachable!()
        };
        s.undo(None);
        s.undo(None);
        assert!(s.tint_ranges().is_empty());
        h.step();
    }

    #[test]
    fn will_clip_reads_a_peak_kept_until_start_or_end_moves() {
        let mut s = session();
        assert!(!s.will_clip());
        // Corrupt the audio behind the cache: only a fresh scan sees the over.
        s.processed[0][100] = 2.0;
        assert!(!s.will_clip(), "not rescanned");
        let e = EditState {
            start: 50,
            ..s.edit().clone()
        };
        s.apply(e, None);
        assert!(s.will_clip(), "rescanned for the new Start");
    }

    #[test]
    fn predicted_peak_is_kept_until_the_question_or_the_edit_changes() {
        let mut s = session();
        let unity = Effect::Gain { db: 0.0 };
        let p = s.predicted_peak(unity.clone(), None).unwrap();
        assert!((p - 0.5).abs() < 1e-3, "{p}");
        s.processed[0][100] = 2.0;
        assert_eq!(
            s.predicted_peak(unity.clone(), None),
            Some(p),
            "not rescanned"
        );
        // A new selection or effect asks again.
        assert_eq!(s.predicted_peak(unity.clone(), Some((0, 200))), Some(2.0));
        let louder = s.predicted_peak(Effect::Gain { db: 6.0 }, Some((0, 200)));
        assert!(louder.is_some_and(|p| p > 3.9), "{louder:?}");
        // So does an edit.
        s.processed[0][100] = 0.0;
        let e = EditState {
            end: 7000,
            ..s.edit().clone()
        };
        s.apply(e, None);
        let p = s.predicted_peak(unity, Some((0, 200))).unwrap();
        assert!(p < 1.0, "{p}");
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

        // The same with a splice in the list.
        s.commit_preview(None);
        add_tail(&mut s, 7000, 2000);
        s.processed[0][0] = 7.0;
        let e = EditState {
            end: 9500,
            ..s.edit().clone()
        };
        s.set_preview(e, None);
        assert_eq!(s.processed[0][0], 7.0);
        assert_eq!(s.shown_edit().end, 9500, "End can sit in the tail");
    }

    #[test]
    fn channel_tool_changes_the_lane_count() {
        let mut s = stereo_session();
        assert_eq!((s.processed.len(), s.peaks.len()), (2, 2));
        assert!(s.apply_effect(Effect::StereoToMono(MonoSource::Mix), None, None));
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

    /// An app with `s` loaded and selected.
    fn app_with(s: Session) -> LimewaveApp {
        let mut app = LimewaveApp::for_test();
        app.files = vec![FileEntry::Loaded(Box::new(s))];
        app.select(0);
        app
    }

    /// A step that splices `audio` (one channel) in place of `first..=last`.
    fn splice_step(first: usize, last: usize, audio: Vec<f32>) -> AppliedEffect {
        let n = audio.len();
        AppliedEffect {
            op: Op::Splice(Arc::new(Splice {
                kind: SpliceKind::SpectrumLoop,
                audio: vec![audio],
                generated: (0, n - 1),
            })),
            first,
            last,
        }
    }

    #[test]
    fn render_key_follows_the_file_and_its_history() {
        assert_eq!(LimewaveApp::for_test().render_key(1), None, "no file");
        let mut app = app_with(session());
        let key = app.render_key(9).unwrap();
        assert_eq!(
            key,
            RenderKey {
                file: 0,
                effects_len: 0,
                history_cursor: 0,
                settings_hash: 9,
            }
        );
        assert!(app.apply_effect(Effect::Gain { db: -6.0 }));
        let after = app.render_key(9).unwrap();
        assert_eq!((after.effects_len, after.history_cursor), (1, 1));
        app.undo();
        assert_eq!(app.render_key(9), Some(key));
    }

    #[test]
    fn render_debounce_defaults_to_200_ms() {
        assert_eq!(
            UiState::default().render_debounce,
            Duration::from_millis(200)
        );
    }

    #[test]
    fn stale_render_is_discarded() {
        let mut app = app_with(session());
        let ctx = app.ctx.clone();
        let key = app.render_key(3).unwrap();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let mut d = TailDialogState::default();
        d.render.start(spawn_render(key, ctx, move || {
            wait.recv().unwrap();
            Err(TailError::NoAudio)
        }));
        app.ui.tail_dialog = Some(d);
        assert!(app.apply_effect(Effect::Gain { db: -6.0 }));
        let history = app.render_key(3).unwrap();
        go.send(()).unwrap();
        app.wait_for_renders();

        let render = &app.ui.tail_dialog.as_ref().unwrap().render;
        assert!(!render.is_rendering(), "the job is drained");
        assert!(render.result(Some(key)).is_none(), "and its result dropped");
        assert!(render.result(app.render_key(3)).is_none());
        assert_eq!(app.render_key(3), Some(history), "the history is unchanged");

        // A job for the current edit is kept.
        let ctx = app.ctx.clone();
        let d = app.ui.tail_dialog.as_mut().unwrap();
        d.render
            .start(spawn_render(history, ctx, || Err(TailError::NoAudio)));
        app.wait_for_renders();
        let render = &app.ui.tail_dialog.as_ref().unwrap().render;
        assert!(matches!(
            render.result(app.render_key(3)),
            Some(Err(TailError::NoAudio))
        ));
    }

    #[test]
    fn spectrum_dialog_renders_are_drained_too() {
        let mut app = app_with(session());
        let key = app.render_key(1).unwrap();
        let mut d = SpectrumDialogState::default();
        d.analysis.start(spawn_render(key, app.ctx.clone(), || {
            Err(SpectrumLoopError::Silent)
        }));
        d.render.start(spawn_render(key, app.ctx.clone(), || {
            Err(SpectrumLoopError::NoAudio)
        }));
        app.ui.spectrum_dialog = Some(d);
        app.wait_for_renders();
        let d = app.ui.spectrum_dialog.as_ref().unwrap();
        assert!(matches!(
            d.analysis.result(Some(key)),
            Some(Err(SpectrumLoopError::Silent))
        ));
        assert!(matches!(
            d.render.result(Some(key)),
            Some(Err(SpectrumLoopError::NoAudio))
        ));
    }

    #[test]
    fn the_new_dialogs_count_as_modal() {
        let mut app = LimewaveApp::for_test();
        assert!(!app.modal_open());
        app.ui.tail_dialog = Some(TailDialogState::default());
        assert!(app.modal_open());
        app.ui.tail_dialog = None;
        app.ui.spectrum_dialog = Some(SpectrumDialogState::default());
        assert!(app.modal_open());
        app.ui.spectrum_dialog = None;
        app.ui.new_dialog = Some(NewDialogState::default());
        assert!(app.modal_open());
    }

    #[test]
    fn preview_uses_its_own_source_and_end_restores_the_session_buffer() {
        // Without an engine: the bookkeeping.
        let mut s = short_session();
        let source = s.source;
        let rendered = s.rendered.clone();
        let step = tail(1, 1000, 900, 500);
        let mut edit = s.edit().clone();
        edit.effects.push(step.clone());
        edit.end = 1499;
        s.preview_step(&step, &edit, None);
        let p = s.dialog_preview.as_ref().expect("a preview is live");
        assert!(p.source != source && p.source != 0, "its own source");
        let mut audio = s.processed.clone();
        apply_effect(&mut audio, &step);
        assert_eq!(p.rendered, render(&audio, &edit), "processed + the step");
        assert_eq!(p.rendered[0].len(), 1500);
        let first = p.source;
        assert_eq!(s.source, source, "the session keeps its source");
        assert_eq!(s.rendered, rendered, "and its audio");
        assert_eq!(s.frames(), 1000);
        assert_eq!(s.history.cursor(), 0, "the history is untouched");
        s.preview_step(&step, &edit, None);
        let second = s.dialog_preview.as_ref().unwrap().source;
        assert!(
            second != first && second != source,
            "a fresh source each time"
        );
        s.end_preview(None);
        assert!(s.dialog_preview.is_none());
        assert_eq!((s.source, &s.rendered), (source, &rendered));

        // With an engine: voices on the preview source move to `previous` when it ends.
        let (engine, mut out) = crate::audio::engine::headless(48_000);
        let mut app = LimewaveApp::build(egui::Context::default(), Some(engine), None);
        app.files = vec![FileEntry::Loaded(Box::new(short_session()))];
        app.select(0);
        let session_source = app.selected_session().unwrap().source;
        out.render(16);
        assert_eq!(out.sources().0, session_source);

        app.preview_step(&step, &edit);
        let preview_source = app
            .selected_session()
            .and_then(|s| s.dialog_preview.as_ref())
            .unwrap()
            .source;
        app.play_preview(950, true);
        let sound = out.render(256);
        assert_eq!(out.sources(), (preview_source, session_source));
        assert_eq!(out.voice_sources(), [preview_source]);
        assert!(sound.iter().any(|x| x.abs() > 0.01), "the preview sounds");

        app.end_preview();
        assert!(app.selected_session().unwrap().dialog_preview.is_none());
        out.render(16);
        assert_eq!(out.sources(), (session_source, preview_source));
        assert_eq!(
            out.voice_sources(),
            [preview_source],
            "fading on its own audio"
        );
        out.render(480);
        assert!(out.voice_sources().is_empty(), "gone after the 5 ms fade");
        // Without a live preview, ending one changes nothing.
        app.end_preview();
        out.render(16);
        assert_eq!(out.sources(), (session_source, preview_source));
    }

    #[test]
    fn switching_files_closes_the_tail_dialog_and_ends_its_preview() {
        let mut app = LimewaveApp::for_test();
        app.files = vec![
            FileEntry::Loaded(Box::new(short_session())),
            FileEntry::Loaded(Box::new(short_session())),
        ];
        app.select(0);
        app.ui.tail_dialog = Some(TailDialogState::default());
        let step = tail(1, 1000, 900, 500);
        let mut edit = app.selected_session().unwrap().edit().clone();
        edit.effects.push(step.clone());
        edit.end = 1499;
        app.preview_step(&step, &edit);
        app.select(1);
        assert!(app.ui.tail_dialog.is_none(), "closed");
        let FileEntry::Loaded(first) = &app.files[0] else {
            unreachable!()
        };
        assert!(first.dialog_preview.is_none(), "the preview ended");
    }

    #[test]
    fn switching_files_closes_the_spectrum_dialog_and_ends_its_preview() {
        let mut app = LimewaveApp::for_test();
        app.files = vec![
            FileEntry::Loaded(Box::new(short_session())),
            FileEntry::Loaded(Box::new(short_session())),
        ];
        app.select(0);
        app.ui.spectrum_dialog = Some(SpectrumDialogState::default());
        let step = tail(1, 1000, 900, 500);
        let mut edit = app.selected_session().unwrap().edit().clone();
        edit.effects.push(step.clone());
        edit.end = 1499;
        app.preview_step(&step, &edit);
        app.select(1);
        assert!(app.ui.spectrum_dialog.is_none(), "closed");
        let FileEntry::Loaded(first) = &app.files[0] else {
            unreachable!()
        };
        assert!(first.dialog_preview.is_none(), "the preview ended");
    }

    #[test]
    fn play_original_range_ends_the_preview_and_plays_the_range_once() {
        let (engine, mut out) = crate::audio::engine::headless(48_000);
        let mut app = LimewaveApp::build(egui::Context::default(), Some(engine), None);
        app.files = vec![FileEntry::Loaded(Box::new(short_session()))];
        app.select(0);
        let session_source = app.selected_session().unwrap().source;
        let step = tail(1, 1000, 900, 500);
        let mut edit = app.selected_session().unwrap().edit().clone();
        edit.effects.push(step.clone());
        edit.end = 1499;
        app.preview_step(&step, &edit);
        app.play_preview(100, true);
        out.render(64);
        app.play_original_range(100, 199);
        assert!(app.selected_session().unwrap().dialog_preview.is_none());
        out.render(480);
        assert_eq!(out.voice_sources(), [session_source], "the original plays");
        // 100 frames at the file's rate, then the voice's release: long gone after a second.
        out.render(48_000);
        assert!(out.voice_sources().is_empty(), "once, not looping");
    }

    #[test]
    fn play_original_ends_the_preview_and_plays_the_session_audio() {
        let (engine, mut out) = crate::audio::engine::headless(48_000);
        let mut app = LimewaveApp::build(egui::Context::default(), Some(engine), None);
        app.files = vec![FileEntry::Loaded(Box::new(short_session()))];
        app.select(0);
        let session_source = app.selected_session().unwrap().source;
        let step = tail(1, 1000, 900, 500);
        let mut edit = app.selected_session().unwrap().edit().clone();
        edit.effects.push(step.clone());
        edit.end = 1499;
        app.preview_step(&step, &edit);
        app.play_preview(100, true);
        out.render(64);
        app.play_original(100);
        assert!(app.selected_session().unwrap().dialog_preview.is_none());
        out.render(480);
        assert_eq!(
            out.sources().0,
            session_source,
            "the session buffer is back"
        );
        assert_eq!(
            out.voice_sources(),
            [session_source],
            "only the original plays once the preview has faded"
        );
    }

    #[test]
    fn play_original_plays_past_end_and_the_loop_once() {
        // A looped file whose End was moved earlier: loop 100..=399, End 499 of 1000 frames.
        let mut s = short_session();
        let mut e = s.edit().clone();
        (e.loop_mode, e.loop_start, e.loop_end, e.end) = (LoopMode::Forward, 100, 399, 499);
        s.apply(e, None);
        let (engine, mut out) = crate::audio::engine::headless(48_000);
        let mut app = LimewaveApp::build(egui::Context::default(), Some(engine), None);
        app.files = vec![FileEntry::Loaded(Box::new(s))];
        app.select(0);
        // From past the loop end: a held note would jump back to the loop start.
        app.play_original(450);
        out.render(400);
        let at = app.cursors()[0].frame;
        assert!(at > 499.0, "plays on past End, to the real ending: {at}");
        // 550 frames, then the release: it ends by itself.
        out.render(48_000);
        assert!(out.voice_sources().is_empty(), "once, not held on the loop");
    }

    #[test]
    fn apply_dialog_edit_clamps_the_selection_and_refits_a_longer_file() {
        // A splice that shortens the file to 600 frames.
        let mut app = app_with(short_session());
        app.ui.selection = Some((550, 800));
        app.ui.play_from = Some(650);
        let mut e = app.selected_session().unwrap().edit().clone();
        e.effects.push(splice_step(500, 999, vec![0.1; 100]));
        e.end = 599;
        let view = ViewTransform {
            first: 0.0,
            frames_per_px: 1.0,
        };
        app.selected_session_mut().unwrap().view = Some(view);
        app.apply_dialog_edit(e);
        let s = app.selected_session().unwrap();
        assert_eq!(s.frames(), 600);
        assert_eq!(s.history.cursor(), 1, "one history entry");
        assert_eq!(app.ui.selection, Some((550, 599)));
        assert_eq!(app.ui.play_from, None);
        assert!(s.view.is_some(), "a shorter file keeps its view");

        // One that lengthens it refits the view.
        let mut e = s.edit().clone();
        e.effects.push(tail(1, 600, 500, 900));
        e.end = 1499;
        app.apply_dialog_edit(e);
        let s = app.selected_session().unwrap();
        assert_eq!(s.frames(), 1500);
        assert_eq!(s.view, None, "refit to show the new audio");
    }
}
