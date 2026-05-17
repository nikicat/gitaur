//! Helpers for parsing libgit2 sideband progress chunks.
//!
//! Servers emit progress lines like
//! `Enumerating objects: 12345\rCounting objects: 100%\r…done.\n`,
//! using `\r` to overwrite the same line and `\n` to commit a phase.

/// Take the last non-empty trimmed line from a sideband chunk, splitting on
/// `\r` *and* `\n` so we capture the current progress state.
pub fn last_line(data: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(data).ok()?;
    s.split(['\r', '\n'])
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(std::string::ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_last_progress_line() {
        assert_eq!(
            last_line(b"Enumerating objects: 100\rCounting objects: 50%\r"),
            Some("Counting objects: 50%".into())
        );
    }

    #[test]
    fn handles_final_newline() {
        assert_eq!(last_line(b"done.\n"), Some("done.".into()));
    }

    #[test]
    fn ignores_empty_and_whitespace() {
        assert_eq!(last_line(b""), None);
        assert_eq!(last_line(b"\n\r\n"), None);
        assert_eq!(last_line(b"   "), None);
    }

    #[test]
    fn handles_invalid_utf8() {
        assert_eq!(last_line(b"\xff\xfe"), None);
    }
}
