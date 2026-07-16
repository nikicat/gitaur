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
//! Three entry points:
//! * [`files`] — a `pacman -U <artifacts>` set (the AUR install lane).
//! * [`sysupgrade`] — a `pacman -Su` with an `--ignore` pin set (the shell's
//!   partial repo-upgrade lane and the `-Syu` passthroughs). Runs against
//!   aurox's rootless synced db ([`alpm_db::open_synced`]) because that is the
//!   post-`-Sy` state the real pacman will resolve against — the system db
//!   only catches up once pacman itself syncs.
//! * [`remove`] — a `pacman -R…` set ([`RemoveRequest`]: the `-R` passthrough
//!   and the shell's removal lane). Runs against the system localdb.
//!
//! [`files`] and [`sysupgrade`] are **advisory**: the synced db can trail what
//! `pacman -Sy` fetches moments later, so a clean preflight doesn't guarantee
//! pacman success and a flagged one can in principle self-heal. Callers gate
//! with an override, never hard-fail — pacman stays the authority. [`remove`]
//! is different: it resolves against the very localdb pacman is about to use,
//! and pacman offers no interactive override for a broken removal — a flagged
//! remove is deterministically doomed, so its caller refuses outright (the
//! pacman-native escape hatches, `-Rdd`/`-Rc`/…, travel into the simulation
//! and preflight clean). Infrastructure failures still fall through to pacman.

use crate::error::{Error, Result};
use crate::names::{Arch, PkgName, PkgTarget};
use crate::pacman::alpm_db;
use crate::version::Version;
use alpm::{Alpm, PrepareData, PrepareError, SigLevel, TransFlag};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use tracing::{debug, instrument};

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
    /// The transaction would remove `removed`, leaving `target`'s dependency
    /// `depend` unsatisfied — the removal twin of [`Issue::UnsatisfiedDep`]
    /// (pacman's "removing X breaks dependency 'd' required by Y" shape).
    RemovalBreaks {
        removed: PkgName,
        depend: PkgTarget,
        target: PkgName,
    },
    /// A `-R` target that is neither an installed package nor an installed
    /// group — pacman's "target not found". Carries the target verbatim
    /// (including any `repo/` prefix), like pacman's own message.
    TargetNotFound { target: PkgTarget },
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
            Self::RemovalBreaks {
                removed,
                depend,
                target,
            } => write!(
                f,
                "removing {removed} breaks dependency '{depend}' required by {target}"
            ),
            Self::TargetNotFound { target } => write!(f, "target not found: {target}"),
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

/// A `pacman -R…` invocation reduced to what the simulation needs.
///
/// Holds the targets and the transaction flags the `-R` modifiers map to,
/// built from a raw passthrough argv by [`RemoveRequest::from_argv`]. That
/// parse is the one site holding the modifier→flag table, so [`remove`]
/// prepares with exactly the semantics pacman will apply (`-Rc` cascades,
/// `-Rdd` skips dep checks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveRequest {
    pub targets: Vec<PkgTarget>,
    pub flags: TransFlag,
}

impl RemoveRequest {
    /// Parse a raw `pacman`-bound argv into a simulate-able remove request.
    ///
    /// `None` means "don't preflight": the argv isn't a remove at all, has no
    /// targets, or carries a flag outside the modeled set — anything that
    /// redirects the db (`--root`, `--dbpath`, `--config`, …), takes a value
    /// this parser doesn't track, or is simply unknown. An unmodeled flag
    /// could make the simulation diverge from what pacman will do, and a
    /// wrong refusal is worse than no preflight, so the unknown case always
    /// stands aside and leaves pacman the authority.
    pub fn from_argv(argv: &[String]) -> Option<Self> {
        let mut is_remove = false;
        let mut flags = TransFlag::NONE;
        // `-s` and `-d` escalate on repetition (`-Rss` recurses through
        // explicitly-installed deps, `-Rdd` drops dep checks entirely), so
        // they're counted rather than or-ed.
        let (mut recursive, mut nodeps) = (0u8, 0u8);
        let mut targets = Vec::new();
        let mut rest = argv.iter();
        while let Some(a) = rest.next() {
            if a == "--" {
                // pacman's end-of-options marker: the rest are targets.
                targets.extend(rest.by_ref().map(PkgTarget::new));
                break;
            }
            if let Some(long) = a.strip_prefix("--") {
                match long {
                    "cascade" => flags |= TransFlag::CASCADE,
                    "recursive" => recursive += 1,
                    "unneeded" => flags |= TransFlag::UNNEEDED,
                    "nosave" => flags |= TransFlag::NO_SAVE,
                    "nodeps" => nodeps += 1,
                    "dbonly" => flags |= TransFlag::DB_ONLY,
                    "noscriptlet" => flags |= TransFlag::NO_SCRIPTLET,
                    // No effect on what trans_prepare would refuse.
                    "noconfirm" | "confirm" | "noprogressbar" | "verbose" | "debug" => {}
                    _ if long.starts_with("color=") => {}
                    // `--color <when>`: consume the value so it isn't
                    // mistaken for a target.
                    "color" => {
                        rest.next()?;
                    }
                    _ => return None,
                }
            } else if let Some(cluster) = a.strip_prefix('-') {
                for c in cluster.chars() {
                    match c {
                        'R' => is_remove = true,
                        'c' => flags |= TransFlag::CASCADE,
                        's' => recursive += 1,
                        'u' => flags |= TransFlag::UNNEEDED,
                        'n' => flags |= TransFlag::NO_SAVE,
                        'd' => nodeps += 1,
                        'v' => {}
                        _ => return None,
                    }
                }
            } else {
                targets.push(PkgTarget::new(a.clone()));
            }
        }
        if !is_remove || targets.is_empty() {
            return None;
        }
        flags |= match recursive {
            0 => TransFlag::NONE,
            1 => TransFlag::RECURSE,
            _ => TransFlag::RECURSE | TransFlag::RECURSE_ALL,
        };
        flags |= match nodeps {
            0 => TransFlag::NONE,
            1 => TransFlag::NO_DEP_VERSION,
            _ => TransFlag::NO_DEP_VERSION | TransFlag::NO_DEPS,
        };
        Some(Self { targets, flags })
    }
}

/// Simulate `pacman -R…` for `req` against the system localdb — the store the
/// real `pacman -R` resolves against.
///
/// Target resolution mirrors pacman's: exact installed pkgname (an optional
/// `local/` prefix allowed), then installed package group. Anything else is
/// [`Issue::TargetNotFound`] — `provides` names deliberately don't resolve,
/// because `pacman -R` doesn't accept them. Not-found targets short-circuit
/// (pacman aborts on them before dependency checking), so the two issue kinds
/// never mix in one result.
#[instrument]
pub fn remove(req: &RemoveRequest) -> Result<Vec<Issue>> {
    let mut alpm = alpm_db::open()?;
    alpm.trans_init(req.flags | TransFlag::NO_LOCK)
        .map_err(|e| Error::other(format!("alpm trans_init: {e}")))?;
    let mut not_found = Vec::new();
    for target in &req.targets {
        let name = target
            .as_str()
            .strip_prefix("local/")
            .unwrap_or(target.as_str());
        let db = alpm.localdb();
        let staged = if let Ok(pkg) = db.pkg(name) {
            alpm.trans_remove_pkg(pkg)
        } else if let Ok(group) = db.group(name) {
            group
                .packages()
                .iter()
                .try_for_each(|pkg| alpm.trans_remove_pkg(pkg))
        } else {
            not_found.push(Issue::TargetNotFound {
                target: target.clone(),
            });
            Ok(())
        };
        // e.g. a duplicated target — not a verdict on the removal, so give up
        // on simulating and let pacman rule on the real argv.
        if let Err(e) = staged {
            alpm.trans_release().ok();
            return Err(Error::other(format!("alpm trans_remove_pkg {target}: {e}")));
        }
    }
    if !not_found.is_empty() {
        alpm.trans_release().ok();
        return Ok(not_found);
    }
    let issues = prepare_issues(&mut alpm);
    alpm.trans_release().ok();
    // In a remove transaction an unsatisfied dep means "removing `causing`
    // orphans `target`'s dependency" — reshape into the removal-verb issue so
    // the message matches what pacman would have printed.
    Ok(issues
        .into_iter()
        .map(|issue| match issue {
            Issue::UnsatisfiedDep {
                target,
                depend,
                causing: Some(removed),
                ..
            } => Issue::RemovalBreaks {
                removed,
                depend,
                target,
            },
            other => other,
        })
        .collect())
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

/// Log each issue as its own structured event so the fields (`pkg1`, `pkg2`,
/// `reason`, …) are queryable in the execution log — the contract
/// `tests/container/smoke/57_pacman_conflict_logged.sh` pins.
///
/// `debug!`, not `warn!`: the console layer shows warnings, and every caller
/// presents the issues to the user in its own voice (the shell's preflight
/// notes, the remove gate's pacman-parity lines, pacman's own error on the
/// passthrough lanes) — a console echo here would say everything twice. The
/// execution log captures `debug` and up, so the structured detail lands
/// there regardless.
pub fn log_issues(issues: &[Issue]) {
    for issue in issues {
        match issue {
            Issue::Conflict { pkg1, pkg2, reason } => debug!(
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
            } => debug!(
                target = target.as_str(),
                depend = depend.as_str(),
                causing_pkg = causing.as_ref().map_or("(none)", PkgName::as_str),
                "pacman preflight: unsatisfied dep",
            ),
            Issue::RemovalBreaks {
                removed,
                depend,
                target,
            } => debug!(
                removed = removed.as_str(),
                depend = depend.as_str(),
                target = target.as_str(),
                "pacman preflight: removal breaks dependency",
            ),
            Issue::TargetNotFound { target } => debug!(
                target = target.as_str(),
                "pacman preflight: target not found",
            ),
            Issue::InvalidArch { pkg, arch } => debug!(
                pkg = pkg.as_str(),
                arch = arch.as_ref().map_or("(unknown)", Arch::as_str),
                "pacman preflight: invalid architecture",
            ),
            Issue::Other { message } => {
                debug!(error = %message, "pacman preflight: prepare failed without detail");
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

    /// The removal twin renders with pacman's remove-mode verb (and, unlike
    /// the install shape, no version — pacman's message carries none).
    #[test]
    fn removal_breaks_phrasing_matches_pacman() {
        let i = Issue::RemovalBreaks {
            removed: PkgName::new("python-pathvalidate"),
            depend: PkgTarget::new("python-pathvalidate>=3.0.0"),
            target: PkgName::new("electron-cash"),
        };
        assert_eq!(
            i.to_string(),
            "removing python-pathvalidate breaks dependency \
             'python-pathvalidate>=3.0.0' required by electron-cash"
        );
    }

    /// Not-found keeps the target verbatim — pacman echoes `core/bash` back
    /// with the prefix, so the preflight must too.
    #[test]
    fn target_not_found_phrasing_matches_pacman() {
        let i = Issue::TargetNotFound {
            target: PkgTarget::new("core/bash"),
        };
        assert_eq!(i.to_string(), "target not found: core/bash");
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn req(parts: &[&str]) -> Option<RemoveRequest> {
        RemoveRequest::from_argv(&argv(parts))
    }

    #[test]
    fn remove_request_parses_bare_remove() {
        let r = req(&["-R", "foo", "bar"]).expect("plain -R parses");
        assert_eq!(
            r.targets,
            vec![PkgTarget::new("foo"), PkgTarget::new("bar")]
        );
        assert_eq!(r.flags, TransFlag::NONE);
    }

    /// Every modeled modifier, in the clustered spelling users actually type.
    #[test]
    fn remove_request_maps_cluster_modifiers() {
        assert_eq!(
            req(&["-Rns", "foo"]).unwrap().flags,
            TransFlag::NO_SAVE | TransFlag::RECURSE
        );
        assert_eq!(req(&["-Rc", "foo"]).unwrap().flags, TransFlag::CASCADE);
        assert_eq!(req(&["-Ru", "foo"]).unwrap().flags, TransFlag::UNNEEDED);
        // Repetition escalates the way pacman's own parser does.
        assert_eq!(
            req(&["-Rss", "foo"]).unwrap().flags,
            TransFlag::RECURSE | TransFlag::RECURSE_ALL
        );
        assert_eq!(
            req(&["-Rd", "foo"]).unwrap().flags,
            TransFlag::NO_DEP_VERSION
        );
        assert_eq!(
            req(&["-Rdd", "foo"]).unwrap().flags,
            TransFlag::NO_DEP_VERSION | TransFlag::NO_DEPS
        );
    }

    /// Long spellings and split clusters accumulate like pacman's getopt —
    /// `-R -s --recursive` counts two recursions.
    #[test]
    fn remove_request_merges_long_and_split_flags() {
        let r = req(&["-R", "--nosave", "-s", "--recursive", "foo"]).unwrap();
        assert_eq!(
            r.flags,
            TransFlag::NO_SAVE | TransFlag::RECURSE | TransFlag::RECURSE_ALL
        );
        // Flags that don't change what prepare would refuse parse but map to
        // nothing.
        assert_eq!(
            req(&["-R", "--noconfirm", "foo"]).unwrap().flags,
            TransFlag::NONE
        );
    }

    /// `--` ends option parsing; `--color`'s value must not be read as a
    /// target in either spelling.
    #[test]
    fn remove_request_handles_terminator_and_color_values() {
        assert_eq!(
            req(&["-R", "--", "--weird-name"]).unwrap().targets,
            vec![PkgTarget::new("--weird-name")]
        );
        assert_eq!(
            req(&["-R", "--color", "never", "foo"]).unwrap().targets,
            vec![PkgTarget::new("foo")]
        );
        assert_eq!(
            req(&["-R", "--color=never", "foo"]).unwrap().targets,
            vec![PkgTarget::new("foo")]
        );
    }

    /// Anything the simulation can't faithfully model refuses to parse:
    /// non-remove ops, no targets, db-redirecting or unknown flags. A wrong
    /// refusal is worse than no preflight.
    #[test]
    fn remove_request_stands_aside_when_not_modelable() {
        assert_eq!(req(&["-Syu"]), None);
        assert_eq!(req(&["-R"]), None);
        assert_eq!(req(&["-Rx", "foo"]), None);
        assert_eq!(req(&["-R", "--root", "/mnt", "foo"]), None);
        assert_eq!(req(&["-R", "--dbpath", "/tmp/db", "foo"]), None);
        assert_eq!(req(&["-R", "--assume-installed", "x", "foo"]), None);
        assert_eq!(req(&["-Rp", "foo"]), None);
    }
}
