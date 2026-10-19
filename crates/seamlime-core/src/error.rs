//! Error types shared by the WAV reading and writing code.

use std::path::PathBuf;

use thiserror::Error;

/// Everything that can go wrong while reading a WAV file.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WavError {
    #[error("not a RIFF file")]
    NotRiff,
    #[error("RIFF file is not a WAVE file")]
    NotWave,
    #[error("RF64 files are not supported")]
    Rf64Unsupported,
    #[error("malformed WAV file: {0}")]
    Malformed(String),
    #[error("unsupported WAV format: {0}")]
    UnsupportedFormat(String),
    #[error("too many channels: {0}")]
    TooManyChannels(u16),
}

/// Everything that can go wrong while loading a WAV file from disk.
#[derive(Debug, Error)]
pub enum LoadError {
    #[error("could not read file: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Wav(#[from] WavError),
}

/// Everything that can go wrong while saving a WAV file. A failed save leaves the target
/// file as it was.
#[derive(Debug, Error)]
pub enum SaveError {
    #[error("could not write file: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not back up the original to {}: {source}", path.display())]
    Backup {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not replace {}: {source}", path.display())]
    Persist {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} has no file name", .0.display())]
    NoFileName(PathBuf),
    #[error("saving to {} would overwrite the original without a backup", .0.display())]
    WouldOverwriteOriginal(PathBuf),
}
