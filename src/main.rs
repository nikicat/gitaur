//! `aurox` binary entry. Initializes tracing + dispatches to [`aurox::cli::run`].

use aurox::{cli, logging, ui};
use std::process::ExitCode;

fn main() -> ExitCode {
    // Held for the whole run: dropping it flushes + closes the trace file.
    let _log_guard = logging::init();

    match cli::run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ExitCode::from(1)
        }
    }
}
