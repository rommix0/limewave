//! The open SFZ instrument: its edit history, which WAV each region plays and where those WAVs
//! sit in the app's file list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use limewave_core::edit::{EditState, History};
use limewave_core::save::Destination;
use limewave_core::sfz::bind::{self, Binding};
use limewave_core::sfz::model::Instrument;
use limewave_core::sfz::paths::same_file_key;

use crate::app::{FileEntry, PendingAction};

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
    /// `binding.samples[i]` -> its index in the app's file list; `None` when it is not there.
    pub sample_file: Vec<Option<usize>>,
    /// What Close instrument opens again: the folder or file open before.
    pub return_to: Option<PendingAction>,
    pub destination: Option<Destination>,
    /// Whether saving also writes the loop and root key into each WAV.
    pub write_smpl: bool,
    /// Whether the key map shows key ranges only, without velocity.
    pub keys_only: bool,
    /// Parallel to `binding.samples`: each loaded WAV's own `initial_edit`, `None` while it
    /// loads or if it failed. Lets [`InstrumentState::edit`] rebind without the file list.
    wavs: Vec<Option<EditState>>,
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
            sample_file: (0..n).map(Some).collect(),
            return_to,
            destination: None,
            write_smpl: false,
            keys_only: false,
            wavs: vec![None; n],
        }
    }

    pub fn inst(&self) -> &Instrument {
        self.history.current()
    }

    /// Records `f` applied to the current instrument as one undo step, then binds the result
    /// again. Samples already open keep their file; one not open maps to `None`.
    pub fn edit(&mut self, f: impl FnOnce(&mut Instrument)) {
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

/// The folder sample paths are relative to.
fn sfz_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}
