//! The open SFZ instrument: its edit history, which WAV each region plays and where those WAVs
//! sit in the app's file list.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use limewave_core::edit::{EditState, History, LoopMode};
use limewave_core::save::Destination;
use limewave_core::sfz::bind::{self, Binding};
use limewave_core::sfz::format_number;
use limewave_core::sfz::map::auto_map;
use limewave_core::sfz::model::{Instrument, Keycenter, SfzLoopMode};
use limewave_core::sfz::parse::{HeaderKind, SfzFile, Token};
use limewave_core::sfz::paths::same_file_key;

use crate::app::{FileEntry, PendingAction, Session};
use crate::audio::voice::{Adsr, InstrumentPlay, Playback, RegionPlay, ampeg_to_adsr};

/// The first source given to instrument regions. Session sources count up from 1 and never
/// get this far, so a region's voice is never taken for a session's.
const REGION_SOURCES: u64 = 1 << 40;
/// Sources each opened instrument sets aside for its regions.
const REGION_SOURCE_BLOCK: u64 = 1 << 20;
/// The next block of region sources.
static NEXT_REGION_SOURCES: AtomicU64 = AtomicU64::new(REGION_SOURCES);

/// Whether `source` belongs to an instrument region rather than a session.
pub fn is_region_source(source: u64) -> bool {
    source >= REGION_SOURCES
}

/// One open instrument (spec 4.5). Its samples are the app's open files.
pub struct InstrumentState {
    /// The `.sfz` file it was read from.
    pub path: PathBuf,
    /// A new instrument that exists only in memory: unsaved until a save gives it a file.
    pub untitled: bool,
    pub history: History<Instrument>,
    /// Which WAV each region of the current instrument plays.
    pub binding: Binding,
    pub selected_region: Option<usize>,
    /// The region to select once the samples have loaded: the one selected when this copy was
    /// saved and reopened.
    pub reselect: Option<usize>,
    /// `binding.samples[i]` -> its index in the app's file list; `None` when it is not there.
    pub sample_file: Vec<Option<usize>>,
    /// What Close instrument opens again: the folder or file open before.
    pub return_to: Option<PendingAction>,
    pub destination: Option<Destination>,
    /// Whether saving also writes the loop and root key into each WAV.
    pub write_smpl: bool,
    /// Whether the key map shows key ranges only, without velocity.
    pub keys_only: bool,
    /// The Unassigned WAVs list with what it was worked out from, so the folders are read again
    /// only when that changes.
    pub unassigned: Option<(Vec<String>, Vec<PathBuf>)>,
    /// Parallel to `binding.samples`: each loaded WAV's own `initial_edit`, `None` while it
    /// loads or if it failed. Lets [`InstrumentState::edit`] rebind without the file list.
    wavs: Vec<Option<EditState>>,
    /// The instrument as a key map drag shows it, not yet in the history.
    pub preview: Option<Instrument>,
    /// Counts preview changes, so the audio follows a drag.
    preview_changes: u64,
    /// Region `n` plays under source `region_sources + n`, the same while the instrument is
    /// edited; also the source of its [`InstrumentPlay`].
    region_sources: u64,
}

impl InstrumentState {
    /// A freshly opened instrument whose bound samples will be files `0..n`, in binding order.
    pub fn new(path: PathBuf, inst: Instrument, return_to: Option<PendingAction>) -> Self {
        let binding = bind::bind(&inst, sfz_dir(&path));
        let n = binding.samples.len();
        InstrumentState {
            path,
            untitled: false,
            history: History::new(inst),
            binding,
            selected_region: None,
            reselect: None,
            sample_file: (0..n).map(Some).collect(),
            return_to,
            destination: None,
            write_smpl: false,
            keys_only: false,
            unassigned: None,
            wavs: vec![None; n],
            preview: None,
            preview_changes: 0,
            region_sources: NEXT_REGION_SOURCES.fetch_add(REGION_SOURCE_BLOCK, Ordering::Relaxed),
        }
    }

    pub fn inst(&self) -> &Instrument {
        self.history.current()
    }

    /// What the key map shows and the engine plays: the drag preview if there is one, else the
    /// current instrument.
    pub fn shown(&self) -> &Instrument {
        self.preview.as_ref().unwrap_or_else(|| self.inst())
    }

    /// Applies `f` to the shown instrument as the drag preview, outside the history.
    pub fn set_preview(&mut self, f: impl FnOnce(&mut Instrument)) {
        let mut next = self.shown().clone();
        f(&mut next);
        self.preview = Some(next);
        self.preview_changes += 1;
    }

    /// Records the drag preview as one undo step, if there is one and it differs from the
    /// instrument (a drag back to where it started is no step).
    pub fn commit_preview(&mut self) {
        if let Some(preview) = self.preview.take() {
            self.preview_changes += 1;
            if preview != *self.inst() {
                self.edit(|i| *i = preview);
            }
        }
    }

    /// The region whose voices play under `source`.
    pub fn region_of_source(&self, source: u64) -> Option<usize> {
        let n = usize::try_from(source.checked_sub(self.region_sources)?).ok()?;
        (n < self.binding.region_sample.len()).then_some(n)
    }

    /// Changes whenever [`play_map`] would: with the instrument, its preview, or the published
    /// audio and edit (drag previews included), source or loading of one of its samples.
    pub fn play_key(&self, files: &[FileEntry]) -> u64 {
        let mut h = DefaultHasher::new();
        (
            self.history.stamp(),
            self.preview_changes,
            self.region_sources,
        )
            .hash(&mut h);
        self.binding.region_sample.len().hash(&mut h);
        for &file in &self.sample_file {
            match file.and_then(|i| files.get(i)) {
                Some(FileEntry::Loaded(s)) => (s.source, s.publishes).hash(&mut h),
                _ => 0u64.hash(&mut h),
            }
        }
        h.finish()
    }

    /// The loaded session of `region`'s sample, and the edit `region` plays it with: the
    /// edit the sample last published (a drag preview included, so edits are heard live) when
    /// the region follows it, else the region's own tuning and loop over the WAV's (spec 4.3).
    fn region_sound<'a>(
        &self,
        inst: &Instrument,
        files: &'a [FileEntry],
        region: usize,
    ) -> Option<(&'a Session, EditState)> {
        let sample = (*self.binding.region_sample.get(region)?)?;
        let Some(FileEntry::Loaded(session)) = files.get((*self.sample_file.get(sample)?)?) else {
            return None;
        };
        let b = &self.binding.samples[sample];
        let follows = b
            .regions
            .iter()
            .position(|&r| r == region)
            .is_none_or(|i| b.follows[i]);
        let edit = if follows {
            session.published.clone()
        } else {
            bind::region_edit(inst, region, &session.doc.initial_edit())
        };
        Some((session, edit))
    }

    /// `region`'s root key: its sample's (or its own, spec 4.3), else its `pitch_keycenter`
    /// while the sample is not loaded, else 60.
    pub fn region_root(&self, files: &[FileEntry], region: usize) -> u8 {
        match self.region_sound(self.inst(), files, region) {
            Some((_, edit)) => edit.root,
            None => match self.inst().region_info(region).keycenter {
                Keycenter::Note(n) => n,
                Keycenter::Sample => 60,
            },
        }
    }

    /// Whether `region`'s sample plays with a loop now (the sample's, or the region's own, spec
    /// 4.3); a sample that is not loaded loops when the region says so.
    pub fn region_loops(&self, files: &[FileEntry], region: usize) -> bool {
        match self.region_sound(self.inst(), files, region) {
            Some((_, edit)) => edit.loop_mode != LoopMode::Off,
            None => matches!(
                self.inst().region_info(region).loop_mode,
                Some(SfzLoopMode::Continuous | SfzLoopMode::Sustain)
            ),
        }
    }

    /// Records `f` applied to the current instrument as one undo step, then binds the result
    /// again. Samples already open keep their file; one not open maps to `None`.
    pub fn edit(&mut self, f: impl FnOnce(&mut Instrument)) {
        // A drag preview was made from the state being changed.
        self.preview = None;
        let mut next = self.inst().clone();
        f(&mut next);
        self.history.push(next);
        let known: HashMap<String, (Option<usize>, Option<EditState>)> = self
            .binding
            .samples
            .iter()
            .zip(self.sample_file.iter().zip(&self.wavs))
            .map(|(s, (&file, wav))| (same_file_key(&s.path), (file, wav.clone())))
            .collect();
        self.binding = bind::bind(self.inst(), sfz_dir(&self.path));
        (self.sample_file, self.wavs) = self
            .binding
            .samples
            .iter()
            .map(|s| {
                known
                    .get(&same_file_key(&s.path))
                    .cloned()
                    .unwrap_or_default()
            })
            .unzip();
        bind::complete(&mut self.binding, self.history.current(), &self.wavs);
    }

    /// Binds the current instrument again and finds each sample in `files` by path.
    pub fn rebind(&mut self, files: &[FileEntry]) {
        self.binding = bind::bind(self.inst(), sfz_dir(&self.path));
        self.sample_file = self
            .binding
            .samples
            .iter()
            .map(|s| {
                let key = same_file_key(&s.path);
                files.iter().position(|f| same_file_key(f.path()) == key)
            })
            .collect();
        self.wavs = self
            .sample_file
            .iter()
            .map(|f| match f.and_then(|i| files.get(i)) {
                Some(FileEntry::Loaded(s)) => Some(s.doc.initial_edit()),
                _ => None,
            })
            .collect();
        bind::complete(&mut self.binding, self.history.current(), &self.wavs);
    }

    /// Whether closing now would lose something: edits to the instrument, an instrument never
    /// saved, or edits to one of its samples.
    pub fn unsaved(&self, files: &[FileEntry]) -> bool {
        self.history.is_dirty()
            || self.untitled
            || self
                .sample_file
                .iter()
                .flatten()
                .any(|&i| files.get(i).is_some_and(FileEntry::is_dirty))
    }

    /// The file-list index of the WAV `region` plays; `None` when it names no sample, the
    /// sample is missing or its file is not open.
    pub fn region_file(&self, region: usize) -> Option<usize> {
        let sample = (*self.binding.region_sample.get(region)?)?;
        *self.sample_file.get(sample)?
    }
}

/// The regions of the shown instrument as the engine plays them (spec 6): each with its
/// effective ranges, level, envelope and playback mode, the audio its sample last published
/// (shared, never rendered or copied here) from `offset` to `end`, and the tuning and loop of
/// the sample as last published, or its
/// own (spec 4.3), with `transpose` folded into the root. Regions whose sample is missing or
/// not loaded, or whose `trigger` isn't `attack`, are left out (spec 6.1).
pub fn play_map(st: &InstrumentState, files: &[FileEntry]) -> InstrumentPlay {
    let inst = st.shown();
    let mut regions = Vec::new();
    for region in 0..inst.regions().len() {
        let Some((session, edit)) = st.region_sound(inst, files, region) else {
            continue;
        };
        let info = inst.region_info(region);
        if !info.attack_trigger {
            continue;
        }
        let mut buffer = session.published_buffer();
        let current = &session.published;
        let last = buffer.audio.first().map_or(0, Vec::len).saturating_sub(1);
        buffer.end = info.end.unwrap_or(current.end).min(current.end);
        buffer.start = info.offset.max(current.start).min(buffer.end);
        buffer.loop_mode = edit.loop_mode;
        buffer.loop_end = edit.loop_end.min(last);
        buffer.loop_start = edit.loop_start.min(buffer.loop_end);
        buffer.root = (i32::from(edit.root) - info.transpose).clamp(0, 127) as u8;
        buffer.cents = edit.cents;
        buffer.source = st.region_sources + region as u64;
        regions.push(RegionPlay {
            buffer,
            lokey: info.lokey,
            hikey: info.hikey,
            lovel: info.lovel,
            hivel: info.hivel,
            gain: 10f32.powf(info.volume / 20.0),
            veltrack: info.amp_veltrack / 100.0,
            playback: match info.loop_mode {
                Some(SfzLoopMode::OneShot) => Playback::OneShot,
                Some(SfzLoopMode::Sustain) => Playback::Sustain,
                _ => Playback::Normal,
            },
            adsr: ampeg_to_adsr(&info.ampeg),
        });
    }
    InstrumentPlay {
        regions,
        source: st.region_sources,
    }
}

/// A new instrument (spec 4.4): one `<group>` and, for each `(sample file name, root key)` in
/// order, a region with `pitch_keycenter` and the key range Auto-map gives it. Line endings
/// are the platform's (spec 7.2). The names must be fit to write (`value_round_trips`).
pub fn new_instrument_model(wavs: &[(String, u8)]) -> Instrument {
    let roots: Vec<u8> = wavs.iter().map(|w| w.1).collect();
    let op = |name: &str, value: String| Token::Opcode {
        name: name.to_owned(),
        value,
    };
    let mut tokens = vec![Token::Header(HeaderKind::Group)];
    for ((name, root), (lo, hi)) in wavs.iter().zip(auto_map(&roots)) {
        tokens.extend([
            Token::Header(HeaderKind::Region),
            op("sample", name.clone()),
            op("pitch_keycenter", root.to_string()),
            op("lokey", lo.to_string()),
            op("hikey", hi.to_string()),
        ]);
    }
    Instrument::new(SfzFile {
        tokens,
        crlf: cfg!(windows),
    })
}

/// Sets `region`'s key range, writing only the ends that change. A key already written as a
/// number stays a number; a note name, or a key the region had no text for, is a note name.
pub fn set_keys(inst: &mut Instrument, region: usize, lo: u8, hi: u8) {
    let info = inst.region_info(region);
    for (name, now, key) in [("lokey", info.lokey, lo), ("hikey", info.hikey, hi)] {
        if now == key {
            continue;
        }
        let existing = inst.get(region, name).map(|(text, _)| text);
        let text = bind::key_text(existing.as_deref(), key, true);
        inst.set(region, name, &text);
    }
}

/// Sets `region`'s velocity range, writing only the ends that change.
pub fn set_velocities(inst: &mut Instrument, region: usize, lo: u8, hi: u8) {
    let info = inst.region_info(region);
    if info.lovel != lo {
        inst.set(region, "lovel", &lo.to_string());
    }
    if info.hivel != hi {
        inst.set(region, "hivel", &hi.to_string());
    }
}

/// The folder sample paths are relative to.
fn sfz_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}

/// Writes number opcode `name` on `region` unless `value` already reads as `current` (what
/// the region would get without the opcode, or has).
fn set_number(inst: &mut Instrument, region: usize, name: &str, value: f64, current: f64) {
    let text = format_number(value);
    if text != format_number(current) {
        inst.set(region, name, &text);
    }
}

/// Sets `region`'s volume in dB, writing only when it differs from what the region has.
pub fn set_volume(inst: &mut Instrument, region: usize, db: f32) {
    let current = inst.region_info(region).volume;
    set_number(inst, region, "volume", db.into(), current.into());
}

/// Sets `region`'s envelope from milliseconds and a 0..1 sustain: `ampeg_*` in seconds and
/// percent, each written only when it differs from what the region has.
pub fn set_envelope(inst: &mut Instrument, region: usize, adsr: &Adsr) {
    let now = inst.region_info(region).ampeg;
    let seconds = |ms: f32| f64::from(ms) / 1000.0;
    set_number(
        inst,
        region,
        "ampeg_attack",
        seconds(adsr.attack_ms),
        now.attack.into(),
    );
    set_number(
        inst,
        region,
        "ampeg_decay",
        seconds(adsr.decay_ms),
        now.decay.into(),
    );
    set_number(
        inst,
        region,
        "ampeg_sustain",
        f64::from(adsr.sustain) * 100.0,
        now.sustain.into(),
    );
    set_number(
        inst,
        region,
        "ampeg_release",
        seconds(adsr.release_ms),
        now.release.into(),
    );
}

/// Sets how `region` plays (spec 5.3). Normal writes `loop_continuous` when its sample
/// `loops` and `no_loop` otherwise; nothing is written when the region already plays so.
pub fn set_playback(inst: &mut Instrument, region: usize, playback: Playback, loops: bool) {
    let (mode, text) = match playback {
        Playback::OneShot => (SfzLoopMode::OneShot, "one_shot"),
        Playback::Sustain => (SfzLoopMode::Sustain, "loop_sustain"),
        Playback::Normal if loops => (SfzLoopMode::Continuous, "loop_continuous"),
        Playback::Normal => (SfzLoopMode::NoLoop, "no_loop"),
    };
    let current = inst.region_info(region).loop_mode;
    // Without `loop_mode` a region already plays as its sample does.
    if current != Some(mode) && !(playback == Playback::Normal && current.is_none()) {
        inst.set(region, "loop_mode", text);
    }
}
