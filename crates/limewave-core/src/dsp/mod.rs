//! Pure signal maths on `&[f32]`: peak pyramids for drawing and zero-crossing search for loop snapping.

pub mod peaks;
pub mod zerocross;

pub use peaks::{PeakPyramid, mono_sum};
pub use zerocross::{
    Crossing, is_crossing, is_upward, loop_crossing, nearest_crossing, nearest_upward,
    snap_loop_end, snap_loop_end_with, snap_loop_start, snap_loop_start_with, step_crossing,
    step_upward,
};
