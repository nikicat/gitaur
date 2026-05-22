//! Interop with system pacman (passthrough exec, alpm DB reads). Version
//! comparison lives on [`crate::version::Ver`] (`<` / `==` invoke vercmp);
//! version-bump *classification* for the upgrade table lives in [`verdiff`].

pub mod alpm_db;
pub mod invoke;
pub mod verdiff;
