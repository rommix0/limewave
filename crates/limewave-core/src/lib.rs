//! Limewave core library: WAV/RIFF handling and loop analysis.

pub mod cue;
pub mod document;
pub mod dsp;
pub mod edit;
pub mod effects;
pub mod error;
pub mod meta;
pub mod notes;
pub mod render;
pub mod riff;
pub mod save;
pub mod sfz;
pub mod smpl;
pub mod spectrum_loop;
pub mod tail;
#[cfg(any(test, feature = "testwav"))]
pub mod testwav;
pub mod wav;

pub use error::{LoadError, SaveError, WavError};

/// The longest new or recorded sample, in seconds.
pub const MAX_NEW_SECONDS: u32 = 600;

/// The software tag written into exported WAV files, e.g. `Limewave 0.7.0`.
pub fn software_tag() -> String {
    format!("Limewave {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_tag_has_name_and_version() {
        assert_eq!(software_tag(), "Limewave 0.7.0");
    }
}
