//! `gitaur` binary entry. Initializes tracing + dispatches to [`gitaur::cli::run`].

use gitaur::{cli, ui};
use std::process::ExitCode;
use tracing_subscriber::{fmt, EnvFilter};

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt().with_env_filter(filter).with_target(false).init();

    match cli::run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ExitCode::from(1)
        }
    }
}
