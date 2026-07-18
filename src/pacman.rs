//! Interop with system pacman (passthrough exec, alpm DB reads).
//!
//! Version comparison lives on [`crate::version::Ver`] (`<` / `==` invoke
//! vercmp); version-bump *classification* for the upgrade table lives in
//! [`verdiff`].

pub mod alpm_db;
pub mod dload;
pub mod invoke;
pub mod preflight;
pub mod sync;
pub mod verdiff;
