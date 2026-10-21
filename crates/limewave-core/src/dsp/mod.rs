//! Pure signal maths on `&[f32]`: peak pyramids for drawing, zero-crossing search for loop
//! snapping, and the analysis building blocks shared by the tail and spectrum-loop effects
//! (seeded randomness, FFT framing, level envelopes, pitch and least-squares fits).

pub mod envelope;
pub mod lsq;
pub mod peaks;
pub mod pitch;
pub mod rng;
pub mod spectrum;
pub mod zerocross;

pub use peaks::{PeakPyramid, mono_sum};
pub use spectrum::{Complex32, Complex64};
pub use zerocross::{
    Crossing, is_crossing, is_upward, loop_crossing, nearest_crossing, nearest_upward,
    snap_loop_end, snap_loop_end_with, snap_loop_start, snap_loop_start_with, step_crossing,
    step_upward,
};
