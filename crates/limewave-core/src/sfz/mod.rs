//! SFZ instrument text: parsing, the instrument model and sample paths.

pub mod bind;
pub mod map;
pub mod model;
pub mod parse;
pub mod paths;
pub mod save;
pub mod write;

pub use bind::{Binding, SampleBinding, bind, complete, region_edit, write_back};
pub use map::{auto_map, fill_gaps};
pub use model::{Ampeg, Instrument, Keycenter, Level, LoopDir, RegionInfo, SfzLoopMode};
pub use parse::{HeaderKind, SfzError, SfzFile, Token, decode_text, expand, parse};
pub use paths::{find_sample, normalize, same_file_key, sample_path, stays_inside};
pub use save::{
    InstrumentSaved, SampleSave, folder_targets, needs_rewrite, save_instrument, smpl_edit_for_save,
};
pub use write::{format_number, value_round_trips, write};
