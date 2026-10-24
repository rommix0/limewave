//! The automatic loop finder (v0.8): searches a sound for the best loop points.
//!
//! This module holds the shared constants and error type. [`steady`] picks the part of the
//! sound worth searching; later stages measure candidate loops and rank them.

pub mod measure;
pub mod steady;

pub use measure::{
    Natural, Quality, SeamMeasures, natural_change, quality, score, seam_measures, tone_frame,
};

/// The longest range the finder will search, in seconds. A longer steady part keeps its last
/// 60 s; a longer selection is refused.
pub const MAX_SEARCH_SECONDS: f64 = 60.0;

/// The most loop candidates the finder returns.
pub const MAX_CANDIDATES: usize = 8;

/// The default shortest loop the finder will propose, in seconds.
pub const DEFAULT_MIN_SECONDS: f64 = 0.1;

/// Why the finder could not produce loops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FinderError {
    /// The range to search is silent.
    Silent,
    /// The range to search is empty or too short to hold a loop.
    RangeTooShort,
    /// The selection is longer than [`MAX_SEARCH_SECONDS`].
    RangeTooLong,
    /// The settings are not usable (for example a minimum length longer than the range).
    BadSettings,
    /// The search was cancelled before it finished.
    Cancelled,
}
