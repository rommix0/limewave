//! Error type shared by the WAV reading and writing code.

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
