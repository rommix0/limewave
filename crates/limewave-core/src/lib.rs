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
pub mod smpl;
#[cfg(any(test, feature = "testwav"))]
pub mod testwav;
pub mod wav;

pub use error::{LoadError, SaveError, WavError};

/// The software tag written into exported WAV files, e.g. `Limewave 0.2.0`.
pub fn software_tag() -> String {
    format!("Limewave {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_tag_has_name_and_version() {
        assert_eq!(
            software_tag(),
            format!("Limewave {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
