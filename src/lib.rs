//! Library entry point for the `cct` crate.
//!
//! The binary at `src/main.rs` is a thin CLI wrapper; everything reusable lives
//! here so integration tests can drive it directly.

pub mod cli;
pub mod commands;
pub mod daemon;
pub mod fs_scan;
pub mod index;
pub mod move_cmd;
pub mod path_codec;
pub mod picker;
pub mod recent;
pub mod rename;
pub mod term;
pub mod text;
