//! Crate-wide error type.

use thiserror::Error;
use toml::de::Error as TomlDeError;

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Unified error variant for all aurox subsystems.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// gix (gitoxide) error from mirror or worktree ops. Wraps the many
    /// per-operation error types gix exposes (each operation has its own).
    #[error("git: {0}")]
    Gix(String),

    /// libalpm error from pacman DB reads.
    #[error("alpm: {0}")]
    Alpm(#[from] alpm::Error),

    /// TOML parse failure from config.
    #[error("toml: {0}")]
    Toml(#[from] TomlDeError),

    /// Regex compile failure (used by -Ss).
    #[error("regex: {0}")]
    Regex(#[from] regex::Error),

    /// Rkyv (de)serialization / validation failure.
    #[error("rkyv: {0}")]
    Rkyv(String),

    /// The on-disk index archive can't be read by this build — either rkyv's
    /// validator rejected the layout or `format_version` predates us. Carries a
    /// human-readable reason; the loader recovers by resyncing the database
    /// rather than surfacing it, so this rarely reaches the user.
    #[error("index incompatible: {0}")]
    IndexIncompatible(String),

    /// `.SRCINFO` parsing failure with offending text.
    #[error("srcinfo parse: {0}")]
    SrcInfo(String),

    /// Dependency resolution failure (cycle, ambiguity, missing).
    #[error("resolve: {0}")]
    Resolve(String),

    /// Build pipeline failure (makepkg, install).
    #[error("build: {0}")]
    Build(String),

    /// pacman exited non-zero with the wrapped exit code.
    #[error("pacman exited with status {0}")]
    PacmanExit(i32),

    /// User declined a confirmation prompt.
    #[error("user aborted")]
    UserAbort,

    /// A makepkg build was interrupted by SIGINT (Ctrl+C). Caught by the build
    /// pipeline and turned into a per-pkgbase "interrupted" outcome rather than
    /// aborting the whole run — the no-arg loop bails back to the table.
    #[error("interrupted")]
    Interrupted,

    /// One or more user-supplied targets were not found.
    #[error("unknown target(s): {0}")]
    UnknownTargets(String),

    /// Catch-all error with a human message.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Construct an [`Error::Other`] from any string-like.
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// Wrap a gix error as [`Error::Gix`], preserving its full `source()` chain.
    ///
    /// gix nests errors deeply and `Display` shows only the outermost layer — a
    /// curl abort inside a fetch surfaces as just "Failed to consume the pack
    /// sent by the remote" while the actual cause (e.g. "curl 28: timeout") sits
    /// several `source()`s down. Use this instead of `Error::Gix(format!(..))`
    /// whenever a gix error is at hand.
    pub fn gix(
        context: impl std::fmt::Display,
        err: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        use std::fmt::Write as _;
        let err = err.into();
        let mut msg = format!("{context}: {err}");
        let mut source = err.source();
        while let Some(cause) = source {
            write!(msg, ": {cause}").expect("writing to a String never fails");
            source = cause.source();
        }
        Self::Gix(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[derive(Debug, thiserror::Error)]
    #[error("failed to consume the pack")]
    struct Outer(#[source] Mid);

    #[derive(Debug, thiserror::Error)]
    #[error("an IO error occurred when reading")]
    struct Mid(#[source] std::io::Error);

    #[test]
    fn gix_preserves_the_source_chain() {
        let err = Outer(Mid(std::io::Error::other("curl 28: Operation too slow")));
        assert_eq!(
            Error::gix("fetch_only", err).to_string(),
            "git: fetch_only: failed to consume the pack: \
             an IO error occurred when reading: curl 28: Operation too slow"
        );
    }
}
