//! Seamlime core library: WAV/RIFF handling and loop analysis.

pub mod error;
pub mod riff;
pub mod smpl;
#[cfg(any(test, feature = "testwav"))]
pub mod testwav;
pub mod wav;

pub use error::WavError;

/// The software tag written into exported WAV files, e.g. `Seamlime 0.1.0`.
pub fn software_tag() -> String {
    format!("Seamlime {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_tag_has_name_and_version() {
        assert_eq!(
            software_tag(),
            format!("Seamlime {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
