//! SQLite-backed record of previously-built pkgbases (used for diff-on-update
//! and rebuild-skip idempotency).

use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use tracing::{debug, instrument};

/// Handle to the build-state database.
pub struct StateDb {
    conn: Connection,
}

/// One row of `builds`.
#[derive(Debug, Clone)]
pub struct BuildRecord {
    /// Hex-encoded commit OID of the branch tip we built from.
    pub last_built_commit_oid: String,
    /// Full `epoch:pkgver-pkgrel` version string we produced.
    pub last_built_version: String,
}

impl StateDb {
    /// Open or create the state DB at `path` and ensure the schema exists.
    #[instrument]
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS builds (
                pkgbase TEXT PRIMARY KEY,
                last_built_commit_oid TEXT NOT NULL,
                last_built_version TEXT NOT NULL,
                built_at INTEGER NOT NULL
            );
            ",
        )?;
        Ok(Self { conn })
    }

    /// Lookup the prior build for `pkgbase`, if any.
    pub fn get(&self, pkgbase: &str) -> Result<Option<BuildRecord>> {
        let row = self
            .conn
            .query_row(
                "SELECT last_built_commit_oid, last_built_version FROM builds WHERE pkgbase = ?1",
                params![pkgbase],
                |r| {
                    Ok(BuildRecord {
                        last_built_commit_oid: r.get(0)?,
                        last_built_version: r.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Upsert a build-success row for `pkgbase`.
    #[instrument(skip(self))]
    pub fn record_build(&mut self, pkgbase: &str, oid_hex: &str, version: &str) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO builds(pkgbase, last_built_commit_oid, last_built_version, built_at)
             VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(pkgbase) DO UPDATE SET
                last_built_commit_oid = excluded.last_built_commit_oid,
                last_built_version = excluded.last_built_version,
                built_at = excluded.built_at",
            params![pkgbase, oid_hex, version, now],
        )?;
        debug!(pkgbase, oid_hex, version, "build recorded");
        Ok(())
    }

    /// Remove rows whose pkgbase is not in `keep`. Used by `-Sc` cleanup.
    pub fn prune(&mut self, keep: &[String]) -> Result<usize> {
        let placeholders = std::iter::repeat_n("?", keep.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = if keep.is_empty() {
            "DELETE FROM builds".to_string()
        } else {
            format!("DELETE FROM builds WHERE pkgbase NOT IN ({placeholders})")
        };
        let n = self
            .conn
            .execute(&sql, rusqlite::params_from_iter(keep.iter()))?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn upsert_then_get() {
        let dir = TempDir::new().unwrap();
        let mut db = StateDb::open(&dir.path().join("state.db")).unwrap();
        assert!(db.get("cower").unwrap().is_none());
        db.record_build("cower", "deadbeef", "17-2").unwrap();
        let r = db.get("cower").unwrap().unwrap();
        assert_eq!(r.last_built_commit_oid, "deadbeef");
        assert_eq!(r.last_built_version, "17-2");
        db.record_build("cower", "feed", "18-1").unwrap();
        assert_eq!(db.get("cower").unwrap().unwrap().last_built_version, "18-1");
    }

    #[test]
    fn prune_keeps_only_listed() {
        let dir = TempDir::new().unwrap();
        let mut db = StateDb::open(&dir.path().join("state.db")).unwrap();
        db.record_build("a", "x", "1-1").unwrap();
        db.record_build("b", "y", "1-1").unwrap();
        db.record_build("c", "z", "1-1").unwrap();
        let n = db.prune(&["b".into()]).unwrap();
        assert_eq!(n, 2);
        assert!(db.get("b").unwrap().is_some());
        assert!(db.get("a").unwrap().is_none());
        assert!(db.get("c").unwrap().is_none());
    }
}
