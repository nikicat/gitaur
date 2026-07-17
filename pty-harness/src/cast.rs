//! Asciicast v2 recording of the raw PTY byte stream.
//!
//! When `PTY_CAST_DIR` is set, every [`crate::Pty`] spawn tees the bytes the
//! reader thread pulls off the PTY into `<dir>/<name>.cast` — a JSON header
//! line followed by one `[elapsed_secs, "o", data]` event per read. The file
//! plays back with `asciinema play` and renders to GIF with `agg`, so a failed
//! e2e scenario can be *watched* instead of reconstructed from a screen dump.
//!
//! `<name>` comes from `PTY_CAST_NAME` (the container runner passes the test
//! script's slug), falling back to the driver binary's name; repeat spawns
//! land on `-2`, `-3`, … via create-new collisions, so parallel test
//! containers sharing one mounted dir never clobber each other.
//!
//! Recording is best-effort by contract: a recorder that fails to initialize
//! or write warns on stderr and disables itself rather than failing the
//! scenario — the cast is a debugging artifact, never the test. Events are
//! written unbuffered so a scenario killed mid-flight still leaves a playable
//! prefix.

use serde_json::json;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// One open `.cast` file plus the state needed to emit well-formed events:
/// the spawn instant (event times are relative to it) and a carry buffer for
/// a UTF-8 sequence split across reads — asciicast event data must be valid
/// UTF-8, and an 8 KiB read boundary happily lands mid-emoji.
pub(crate) struct CastRecorder {
    out: File,
    start: Instant,
    carry: Vec<u8>,
}

impl CastRecorder {
    /// Build a recorder if `PTY_CAST_DIR` is set, else `None`. Initialization
    /// failure (unwritable dir, exhausted names) warns and returns `None` —
    /// see the module doc's best-effort contract.
    pub(crate) fn from_env(title: &str) -> Option<Self> {
        let dir = std::env::var_os("PTY_CAST_DIR")?;
        let name = std::env::var("PTY_CAST_NAME").unwrap_or_else(|_| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "pty".to_owned())
        });
        match Self::create(Path::new(&dir), &name, title) {
            Ok(rec) => Some(rec),
            Err(err) => {
                eprintln!("pty-harness: cast recording disabled: {err}");
                None
            }
        }
    }

    fn create(dir: &Path, name: &str, title: &str) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut out = None;
        for i in 1..=99 {
            let file = if i == 1 {
                format!("{name}.cast")
            } else {
                format!("{name}-{i}.cast")
            };
            match File::options()
                .write(true)
                .create_new(true)
                .open(dir.join(file))
            {
                Ok(f) => {
                    out = Some(f);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        let Some(mut out) = out else {
            return Err(io::Error::other(format!(
                "99 casts named {name}* already exist"
            )));
        };
        let header = json!({
            "version": 2,
            "width": crate::COLS,
            "height": crate::ROWS,
            "timestamp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            "title": title,
            "env": {"TERM": "xterm-256color"},
        });
        writeln!(out, "{header}")?;
        Ok(Self {
            out,
            start: Instant::now(),
            carry: Vec::new(),
        })
    }

    /// Append one output event for `bytes` as read off the PTY. Bytes that
    /// end in a truncated UTF-8 sequence are carried into the next call.
    pub(crate) fn record(&mut self, bytes: &[u8]) -> io::Result<()> {
        let elapsed = self.start.elapsed().as_secs_f64();
        let joined;
        let buf: &[u8] = if self.carry.is_empty() {
            bytes
        } else {
            joined = [self.carry.as_slice(), bytes].concat();
            &joined
        };
        let (text, tail) = decode_prefix(buf);
        self.carry = tail.to_vec();
        if text.is_empty() {
            return Ok(());
        }
        self.write_event(elapsed, &text)
    }

    /// Flush any carried bytes (lossily — the stream ended mid-sequence, so
    /// there is no next chunk to complete it). Call once at stream end.
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if self.carry.is_empty() {
            return Ok(());
        }
        let elapsed = self.start.elapsed().as_secs_f64();
        let text = String::from_utf8_lossy(&self.carry).into_owned();
        self.carry.clear();
        self.write_event(elapsed, &text)
    }

    fn write_event(&mut self, elapsed: f64, data: &str) -> io::Result<()> {
        // Millisecond precision keeps lines short; players don't resolve finer.
        let t = (elapsed * 1000.0).round() / 1000.0;
        writeln!(self.out, "{}", json!([t, "o", data]))
    }
}

/// Split `buf` into the longest decodable prefix (decidably-invalid sequences
/// become U+FFFD) and a trailing *incomplete* sequence to retry once the next
/// chunk arrives. Only a truncated tail is carried — by UTF-8's structure it
/// is at most 3 bytes, so the carry can never grow unboundedly.
fn decode_prefix(buf: &[u8]) -> (String, &[u8]) {
    let mut text = String::new();
    let mut rest = buf;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                text.push_str(s);
                return (text, &[]);
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                text.push_str(std::str::from_utf8(valid).expect("validated prefix"));
                match e.error_len() {
                    None => return (text, after),
                    Some(n) => {
                        text.push('\u{FFFD}');
                        rest = &after[n..];
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn decode_prefix_passes_clean_utf8_through() {
        let (text, tail) = decode_prefix("plain → 📥".as_bytes());
        assert_eq!(text, "plain → 📥");
        assert!(tail.is_empty());
    }

    #[test]
    fn decode_prefix_carries_truncated_tail() {
        let emoji = "📥".as_bytes(); // 4 bytes
        let (text, tail) = decode_prefix(&emoji[..2]);
        assert_eq!(text, "");
        assert_eq!(tail, &emoji[..2]);
    }

    #[test]
    fn decode_prefix_replaces_decidably_invalid_bytes() {
        let (text, tail) = decode_prefix(b"ok\xffok");
        assert_eq!(text, "ok\u{FFFD}ok");
        assert!(tail.is_empty());
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("read cast")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid JSON line"))
            .collect()
    }

    #[test]
    fn recorder_reassembles_split_emoji_across_chunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut rec = CastRecorder::create(dir.path(), "split", "t").expect("create");
        let bytes = "a📥b".as_bytes();
        rec.record(&bytes[..2]).expect("record head");
        rec.record(&bytes[2..]).expect("record tail");
        rec.finish().expect("finish");

        let lines = read_lines(&dir.path().join("split.cast"));
        assert_eq!(lines[0]["version"], 2);
        assert_eq!(lines[0]["width"], u64::from(crate::COLS));
        assert_eq!(lines[0]["height"], u64::from(crate::ROWS));
        // Event 1 is the valid prefix "a"; event 2 reunites the emoji.
        assert_eq!(lines[1][1], "o");
        assert_eq!(lines[1][2], "a");
        assert_eq!(lines[2][2], "📥b");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn recorder_finish_flushes_carry_lossily() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut rec = CastRecorder::create(dir.path(), "lossy", "t").expect("create");
        rec.record(&"📥".as_bytes()[..2]).expect("record");
        rec.finish().expect("finish");

        let lines = read_lines(&dir.path().join("lossy.cast"));
        assert_eq!(lines[1][2], "\u{FFFD}");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn second_recorder_with_same_name_gets_suffixed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _first = CastRecorder::create(dir.path(), "dup", "t").expect("first");
        let _second = CastRecorder::create(dir.path(), "dup", "t").expect("second");
        assert!(dir.path().join("dup.cast").exists());
        assert!(dir.path().join("dup-2.cast").exists());
    }
}
