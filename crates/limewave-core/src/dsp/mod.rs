//! Pure signal maths on `&[f32]`: peak pyramids for drawing and zero-crossing search for loop snapping.

pub mod peaks;
pub mod zerocross;

pub use peaks::{PeakPyramid, mono_sum};
pub use zerocross::{is_upward, nearest_upward, snap_loop_end, snap_loop_start, step_upward};
