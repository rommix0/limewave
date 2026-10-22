//! SFZ instrument text: parsing and (in later modules) the instrument model.

pub mod parse;

pub use parse::{HeaderKind, SfzError, SfzFile, Token, decode_text, expand, parse};
