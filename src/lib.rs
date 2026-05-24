//! `gitaur` library crate. Same module tree as the binary; `main.rs` thin-wraps
//! [`cli::run`]. Exposed here so `tests/` integration suites can drive
//! individual layers (mirror, index, resolver) directly.

pub mod build;
pub mod cli;
pub mod config;
pub mod error;
pub mod index;
pub mod logging;
pub mod mirror;
pub mod names;
pub mod pacman;
pub mod paths;
pub mod resolver;
pub mod runopts;
pub mod ui;
pub mod version;

#[doc(hidden)]
pub mod testing;
