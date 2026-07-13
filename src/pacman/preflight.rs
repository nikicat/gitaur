//! Read-only libalpm transaction preflights.
//!
//! Simulate the transaction pacman is about to run (`trans_init(NO_LOCK)` →
//! stage → `trans_prepare` → `trans_release`, never a commit) so dependency
//! breakage and conflicts surface *before* any confirm prompt, sudo
//! escalation, or privileged `pacman` run — and land in the execution log as
//! structured events either way. The motivating failure: a full `-Syu` that
//! dies at pacman's own prepare with "installing libjpeg-turbo breaks
//! dependency 'libjpeg' required by ioquake3-git" *after* the user already
//! confirmed a multi-GiB transaction and typed the sudo password.
//!
//! Two entry points:
//! * [`files`] — a `pacman -U <artifacts>` set (the AUR install lane).
//! * [`sysupgrade`] — a `pacman -Su` with an `--ignore` pin set (the shell's
//!   partial repo-upgrade lane and the `-Syu` passthroughs). Runs against
//!   aurox's rootless synced db ([`alpm_db::open_synced`]) because that is the
//!   post-`-Sy` state the real pacman will resolve against — the system db
//!   only catches up once pacman itself syncs.
//!
//! Both are **advisory**: the synced db can trail what `pacman -Sy` fetches
//! moments later, so a clean preflight doesn't guarantee pacman success and a
//! flagged one can in principle self-heal. Callers gate with an override,
//! never hard-fail — pacman stays the authority.

use crate::error::{Error, Result};
use crate::names::{Arch, PkgName, PkgTarget};
use crate::pacman::alpm_db;
use crate::version::Version;
use alpm::{Alpm, PrepareData, PrepareError, SigLevel, TransFlag};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use tracing::{instrument, warn};

/// One problem `trans_prepare` found with the simulated transaction, with the
/// participants extracted as owned, typed data (a [`PrepareError`] borrows the
/// alpm handle, so it can't leave this module).
///
/// `PartialEq` only (no `Eq`): [`Version`] equality is vercmp's, which is not
/// derivable-`Eq` material.
#[derive(Debug, Clone, PartialEq)]
pub enum Issue {
    /// Two packages in the prepared transaction conflict. `reason` is the
    /// dep-spec the conflict was declared through (`conflicts=<spec>`) — it
    /// names a third party when the collision is via a virtual/provides.
    Conflict {
        pkg1: PkgName,
        pkg2: PkgName,
        reason: PkgTarget,
    },
    /// The transaction would leave `target`'s dependency `depend` unsatisfied.
    /// `causing` is the package whose install/upgrade removes the provider —
    /// pacman's "installing X breaks dependency 'd' required by Y" shape;
    /// `None` models the plain "can't satisfy 'd'" shape.
    UnsatisfiedDep {
        target: PkgName,
        depend: PkgTarget,
        causing: Option<PkgName>,
        /// `causing`'s sync-repo version, for the pacman-parity message.
        /// Filled best-effort after the prepare; `None` when unavailable.
        causing_ver: Option<Version>,
    },
    /// A package whose `arch` doesn't match this machine.
    InvalidArch { pkg: PkgName, arch: Option<Arch> },
    /// The prepare failed without structured detail — carried verbatim so the
    /// caller can still show *something* actionable.
    Other { message: String },
}

impl fmt::Display for Issue {
    /// Pacman's own phrasing, so the early warning reads exactly like the
    /// error pacman would otherwise print after the sudo prompt.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { pkg1, pkg2, reason } => {
                // pacman prints the reason only when it names a third party
                // (a virtual/provides), not when it restates the pair.
                if reason.refers_to(pkg1) || reason.refers_to(pkg2) {
                    write!(f, "{pkg1} and {pkg2} are in conflict")
                } else {
                    write!(f, "{pkg1} and {pkg2} are in conflict ({reason})")
                }
            }
            Self::UnsatisfiedDep {
                target,
                depend,
                causing: Some(causing),
                causing_ver,
            } => match causing_ver {
                Some(v) => write!(
                    f,
                    "installing {causing} ({v}) breaks dependency '{depend}' required by {target}"
                ),
                None => write!(
                    f,
                    "installing {causing} breaks dependency '{depend}' required by {target}"
                ),
            },
            Self::UnsatisfiedDep {
                target,
                depend,
                causing: None,
                ..
            } => write!(
                f,
                "unable to satisfy dependency '{depend}' required by {target}"
            ),
            Self::InvalidArch { pkg, arch } => match arch {
                Some(a) => write!(f, "package {pkg} does not have a valid architecture ({a})"),
                None => write!(f, "package {pkg} does not have a valid architecture"),
            },
            Self::Other { message } => write!(f, "{message}"),
        }
    }
}

/// Simulate the `pacman -Su` half of a sysupgrade with `ignore` pinned (the
/// `--ignore=<csv>` set of a partial upgrade).
///
/// Returns whatever `trans_prepare` would refuse; empty means the upgrade
/// prepares cleanly.
///
/// Read-only and rootless: `NO_LOCK` skips `db.lck` (the real pacman takes it
/// for the actual write), and nothing is committed or downloaded. pacman.conf's
/// own `IgnorePkg` entries are already honored — `alpm_with_conf` loads them.
#[instrument]
pub fn sysupgrade(ignore: &[PkgName]) -> Result<Vec<Issue>> {
    let mut alpm = alpm_db::open_synced()?;
    for name in ignore {
        alpm.add_ignorepkg(name.as_str())
            .map_err(|e| Error::other(format!("alpm add_ignorepkg {name}: {e}")))?;
    }
    alpm.trans_init(TransFlag::NO_LOCK)
        .map_err(|e| Error::other(format!("alpm trans_init: {e}")))?;
    if let Err(e) = alpm.sync_sysupgrade(false) {
        alpm.trans_release().ok();
        return Err(Error::other(format!("alpm sync_sysupgrade: {e}")));
    }
    let mut issues = prepare_issues(&mut alpm);
    alpm.trans_release().ok();
    // The pacman-parity message wants the breaking package's new version;
    // `extract_issues` couldn't read it (the PrepareError held the handle
    // borrowed), so enrich in a second pass. Best-effort by design.
    for issue in &mut issues {
        if let Issue::UnsatisfiedDep {
            causing: Some(causing),
            causing_ver: causing_ver @ None,
            ..
        } = issue
        {
            *causing_ver = sync_version(&alpm, causing);
        }
    }
    Ok(issues)
}

/// Simulate `pacman -U <paths>` (with `--needed` semantics).
///
/// Returns whatever `trans_prepare` would refuse. The `-U` twin of
/// [`sysupgrade`], against the *system* db — that's the store the real
/// `pacman -U` resolves against.
pub fn files(paths: &[&Path]) -> Result<Vec<Issue>> {
    let mut alpm = alpm_db::open()?;
    // NEEDED mirrors the real `--needed` flag so we don't flag a conflict for
    // a same-version reinstall that pacman would silently skip.
    alpm.trans_init(TransFlag::NO_LOCK | TransFlag::NEEDED)
        .map_err(|e| Error::other(format!("alpm trans_init: {e}")))?;
    for path in paths {
        // Byte-preserving cast for libalpm's C-string path (artifact paths
        // aren't guaranteed UTF-8); `display()` is for the message only.
        let loaded = alpm
            .pkg_load(path.as_os_str().as_bytes(), true, SigLevel::NONE)
            .map_err(|e| Error::other(format!("alpm pkg_load {}: {e}", path.display())))?;
        alpm.trans_add_pkg(loaded).map_err(|e| {
            Error::other(format!(
                "alpm trans_add_pkg {}: {}",
                path.display(),
                e.error
            ))
        })?;
    }
    let issues = prepare_issues(&mut alpm);
    alpm.trans_release().ok();
    Ok(issues)
}

/// Run `trans_prepare` on the staged transaction and widen its complaint list
/// into owned [`Issue`]s. `PrepareError` borrows the handle mutably, so the
/// extraction happens here and only owned data leaves.
fn prepare_issues(alpm: &mut Alpm) -> Vec<Issue> {
    match alpm.trans_prepare() {
        Ok(()) => Vec::new(),
        Err(prep_err) => extract_issues(&prep_err),
    }
}

/// Widen each prepare-time complaint into an owned [`Issue`].
fn extract_issues(err: &PrepareError<'_>) -> Vec<Issue> {
    let Some(data) = err.data() else {
        return vec![Issue::Other {
            message: err.to_string(),
        }];
    };
    match data {
        PrepareData::ConflictingDeps(list) => list
            .iter()
            .map(|c| Issue::Conflict {
                pkg1: PkgName::new(c.package1().name()),
                pkg2: PkgName::new(c.package2().name()),
                reason: PkgTarget::new(c.reason().to_string()),
            })
            .collect(),
        PrepareData::UnsatisfiedDeps(list) => list
            .iter()
            .map(|m| Issue::UnsatisfiedDep {
                target: PkgName::new(m.target()),
                depend: PkgTarget::new(m.depend().to_string()),
                causing: m.causing_pkg().map(PkgName::new),
                causing_ver: None,
            })
            .collect(),
        PrepareData::PkgInvalidArch(list) => list
            .iter()
            .map(|p| Issue::InvalidArch {
                pkg: PkgName::new(p.name()),
                arch: p.arch().map(Arch::new),
            })
            .collect(),
    }
}

/// `name`'s version in the first sync DB declaring it (pacman.conf order) —
/// the same first-db-wins precedence pacman resolves with.
fn sync_version(alpm: &Alpm, name: &PkgName) -> Option<Version> {
    alpm.syncdbs()
        .iter()
        .find_map(|db| db.pkg(name.as_str()).ok())
        .map(|p| Version::from(p.version()))
}

/// Log each issue as its own structured warn event so the fields (`pkg1`,
/// `pkg2`, `reason`, …) are queryable in the execution log — the contract
/// `tests/container/smoke/57_pacman_conflict_logged.sh` pins.
pub fn log_issues(issues: &[Issue]) {
    for issue in issues {
        match issue {
            Issue::Conflict { pkg1, pkg2, reason } => warn!(
                pkg1 = pkg1.as_str(),
                pkg2 = pkg2.as_str(),
                reason = reason.as_str(),
                "pacman preflight: conflict detected",
            ),
            Issue::UnsatisfiedDep {
                target,
                depend,
                causing,
                ..
            } => warn!(
                target = target.as_str(),
                depend = depend.as_str(),
                causing_pkg = causing.as_ref().map_or("(none)", PkgName::as_str),
                "pacman preflight: unsatisfied dep",
            ),
            Issue::InvalidArch { pkg, arch } => warn!(
                pkg = pkg.as_str(),
                arch = arch.as_ref().map_or("(unknown)", Arch::as_str),
                "pacman preflight: invalid architecture",
            ),
            Issue::Other { message } => {
                warn!(error = %message, "pacman preflight: prepare failed without detail");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaks(causing_ver: Option<&str>) -> Issue {
        Issue::UnsatisfiedDep {
            target: PkgName::new("ioquake3-git"),
            depend: PkgTarget::new("libjpeg"),
            causing: Some(PkgName::new("libjpeg-turbo")),
            causing_ver: causing_ver.map(Version::new),
        }
    }

    /// The headline case renders exactly like pacman's own error, so the early
    /// warning is recognizable as "the thing pacman would have said".
    #[test]
    fn breaks_dependency_phrasing_matches_pacman() {
        assert_eq!(
            breaks(Some("3.2.0-2")).to_string(),
            "installing libjpeg-turbo (3.2.0-2) breaks dependency 'libjpeg' \
             required by ioquake3-git"
        );
        assert_eq!(
            breaks(None).to_string(),
            "installing libjpeg-turbo breaks dependency 'libjpeg' required by ioquake3-git"
        );
    }

    #[test]
    fn unsatisfied_without_causing_pkg() {
        let i = Issue::UnsatisfiedDep {
            target: PkgName::new("foo"),
            depend: PkgTarget::new("libbar>=2"),
            causing: None,
            causing_ver: None,
        };
        assert_eq!(
            i.to_string(),
            "unable to satisfy dependency 'libbar>=2' required by foo"
        );
    }

    /// The reason is shown only when it names a third party (a virtual /
    /// provides), matching pacman's conflict message shapes. A versioned
    /// `conflicts=foo>=2` spec still counts as restating the pair.
    #[test]
    fn conflict_reason_shown_only_when_it_adds_information() {
        let conflict = |reason: &str| Issue::Conflict {
            pkg1: PkgName::new("foo"),
            pkg2: PkgName::new("bar"),
            reason: PkgTarget::new(reason),
        };
        assert_eq!(conflict("bar").to_string(), "foo and bar are in conflict");
        assert_eq!(
            conflict("bar>=2").to_string(),
            "foo and bar are in conflict"
        );
        assert_eq!(
            conflict("libbaz").to_string(),
            "foo and bar are in conflict (libbaz)"
        );
    }

    #[test]
    fn invalid_arch_with_and_without_arch() {
        let with = Issue::InvalidArch {
            pkg: PkgName::new("foo"),
            arch: Some(Arch::new("armv7h")),
        };
        assert_eq!(
            with.to_string(),
            "package foo does not have a valid architecture (armv7h)"
        );
        let without = Issue::InvalidArch {
            pkg: PkgName::new("foo"),
            arch: None,
        };
        assert_eq!(
            without.to_string(),
            "package foo does not have a valid architecture"
        );
    }

    /// The unstructured fallback carries libalpm's message verbatim — no
    /// rephrasing that could hide what alpm actually said.
    #[test]
    fn other_passes_the_message_through() {
        let other = Issue::Other {
            message: "transaction not prepared".to_owned(),
        };
        assert_eq!(other.to_string(), "transaction not prepared");
    }
}
