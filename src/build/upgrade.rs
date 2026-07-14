//! `aurox -Qu` and the shared upgrade-query plumbing that also feeds the
//! `-Syu` interactive picker. Read-only: walks alpm + the AUR index file,
//! never shells out to `pacman -S` or asks for sudo.

use crate::config::Config;
use crate::error::Result;
use crate::index::schema::IndexEntry;
use crate::index::secondary::{AurClass, Secondary};
use crate::index::{self, IndexFile};
use crate::names::{PkgBase, PkgName};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke::{self, PkgUpgrade};
use crate::paths;
use crate::ui;
use tracing::{instrument, warn};

/// Whether `--devel` is in effect for an upgrade scan.
///
/// VCS pkgbases (`-git`/`-svn`/`-hg`/`-bzr`) carry no upstream-comparable
/// version until `makepkg` rebuilds them, so plain vercmp never sees a new
/// upstream commit. `--devel` (yay/paru parity) opts into rebuilding them
/// anyway; the default leaves them alone. A named pair instead of a bare `bool`
/// so call sites read `DevelPolicy::Rebuild`, not a nameless `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevelPolicy {
    /// Default: a VCS pkgbase upgrades only when vercmp says it's outdated.
    Skip,
    /// `--devel`: force every VCS pkgbase into the upgrade set for a rebuild.
    Rebuild,
}

impl DevelPolicy {
    /// Collapse the CLI/config `--devel` toggle (the flag ∪ config value, plus
    /// `-Qu`'s trailing `--devel`) into the policy.
    pub const fn from_enabled(enabled: bool) -> Self {
        if enabled { Self::Rebuild } else { Self::Skip }
    }

    /// Whether a VCS pkgbase is forced into the upgrade set regardless of vercmp.
    const fn rebuilds_vcs(self) -> bool {
        matches!(self, Self::Rebuild)
    }
}

/// `aurox -Qu` — show the union of pacman-repo and AUR upgrade candidates.
///
/// One flat, severity-sorted table grouped by `repo` column. Read-only and
/// unprivileged (no sudo), so safe to call both as the bare `-Qu` and as a
/// preview before `-Syu` runs.
#[instrument]
pub fn cmd_query_upgrades(cfg: &Config, devel: DevelPolicy) -> Result<u8> {
    ui::upgrade_table(&collect_upgrade_plan(cfg, devel)?);
    Ok(0)
}

/// Gather the merged repo + AUR upgrade list. Shared by `-Qu` (read-only
/// rendering) and `-Syu` (feeds the interactive picker). Unprivileged —
/// reads alpm and the AUR index file only.
pub fn collect_upgrade_plan(cfg: &Config, devel: DevelPolicy) -> Result<Vec<PkgUpgrade>> {
    // With no AUR data the session loads empty and the recompute naturally
    // yields repo upgrades only — same path either way.
    UpgradeSession::load(cfg)?.recompute_remaining(devel)
}

/// The once-per-session immutable state behind the no-arg `aurox` upgrade loop.
///
/// Holds the AUR index plus its secondary lookup maps, loaded exactly once.
/// The mirror is fetched at session start and never re-fetched mid-loop, so
/// this view is fixed for the whole session — only the localdb changes as
/// packages get installed. Each iteration calls [`Self::recompute_remaining`],
/// which re-snapshots alpm (cheap) and recomputes the candidate list against
/// this frozen index.
///
/// `-Qu` and the non-interactive single-shot path build a session, recompute
/// once, and drop it — see [`collect_upgrade_plan`].
pub struct UpgradeSession {
    idx: IndexFile,
    by: Secondary,
}

impl UpgradeSession {
    /// Load the AUR index + secondary maps once. **This is the one seam where
    /// AUR availability affects data flow**: when AUR data is unavailable —
    /// never synced, or `aur = false` — the session loads *empty*, so every
    /// downstream path (search, classify, resolve, upgrade) runs uniformly
    /// and the AUR simply contributes no rows. Wording decisions consult
    /// [`index::AurState`] instead of branching here.
    pub fn load(cfg: &Config) -> Result<Self> {
        if index::AurState::probe(cfg) != index::AurState::Ready {
            return Ok(Self::empty());
        }
        let idx = index::load_or_resync(cfg, &paths::index_path())?;
        let by = Secondary::build(&idx);
        Ok(Self { idx, by })
    }

    /// A session with zero AUR entries — the pacman-only / not-yet-synced view.
    pub fn empty() -> Self {
        let idx = IndexFile::empty();
        let by = Secondary::build(&idx);
        Self { idx, by }
    }

    /// The loaded AUR index — immutable for the session.
    pub const fn index(&self) -> &IndexFile {
        &self.idx
    }

    /// The secondary lookup maps over [`Self::index`].
    pub const fn secondary(&self) -> &Secondary {
        &self.by
    }

    /// The AUR pkgbase that owns a foreign localdb pkgname, or `None` when the
    /// name isn't in the index. Lets the loop map an AUR upgrade row (keyed by
    /// pkgname) back to the pkgbase its session badges are keyed on.
    pub fn pkgbase_of(&self, name: &PkgName) -> Option<&PkgBase> {
        match self.by.classify_foreign(&self.idx, name) {
            AurClass::AsPkgname(e) | AurClass::AsProvides(e) | AurClass::AsPkgbase(e) => {
                Some(&e.pkgbase)
            }
            AurClass::NotInAur => None,
        }
    }

    /// Re-snapshot the localdb and recompute the remaining upgrade candidates
    /// (repo ∪ AUR) against the frozen index.
    ///
    /// Opens one alpm handle so both halves see one consistent localdb view —
    /// the per-iteration cost the loop pays (≈10ms), as opposed to the
    /// once-only index load above.
    #[instrument(skip(self))]
    pub fn recompute_remaining(&self, devel: DevelPolicy) -> Result<Vec<PkgUpgrade>> {
        // Same rootless-sync db `query_repo_upgrades` uses, so the AUR-vs-repo
        // "foreign" split is computed against one consistent view.
        let alpm = alpm_db::open_synced()?;
        let mut plan = invoke::query_repo_upgrades_in(&alpm);
        let pac = PacmanIndex::build(&alpm);
        plan.extend(aur_upgrades(&self.idx, &self.by, &pac, devel));
        Ok(plan)
    }
}

/// Scan the localdb for foreign pkgs with a newer version in the AUR index.
///
/// [`DevelPolicy::Rebuild`] forces every VCS pkgbase (`-git`/`-svn`/`-hg`/
/// `-bzr`) into the list regardless of vercmp, since their `pkgver` is only
/// refreshed by `makepkg`. Under [`DevelPolicy::Skip`] VCS pkgs are skipped
/// (their on-disk version always looks stale).
fn aur_upgrades(
    idx: &IndexFile,
    by: &Secondary,
    pac: &PacmanIndex,
    devel: DevelPolicy,
) -> Vec<PkgUpgrade> {
    // An empty index (AUR not in play this run) can't answer anything —
    // skip the foreign scan entirely so the per-pkg "not in AUR index"
    // warns below don't fire for every foreign package.
    if idx.entries.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (name, installed_ver) in pac.foreign() {
        // Cross-domain classifier: pacman has `name` as a localdb pkgname;
        // we ask AUR how it relates — own pkgname / provides / pkgbase /
        // unknown. The provides arm is what surfaces dotnet-style
        // renames (foreign `dotnet-runtime-7.0` matched by an AUR pkg's
        // `provides=` declaration).
        use crate::index::secondary::AurClass;
        let entry = match by.classify_foreign(idx, &name) {
            AurClass::AsPkgname(e) => e,
            // The cross-rename path (provides/pkgbase match): the candidate
            // pkgbase isn't named after the installed pkg, it just declares it
            // satisfies the same virtual. Some such pkgs ALSO declare
            // `conflicts=<installed-name>` without `replaces=<installed-name>`
            // — a replacement that needs the user's explicit consent, not a
            // transparent upgrade. Pacman would prompt "Remove X?" and abort
            // under `--noconfirm`. Skip with a structured warn so the user
            // can find these orphans in the log and opt in by hand.
            AurClass::AsProvides(e) | AurClass::AsPkgbase(e) => {
                if !is_transparent_upgrade_for(e, &name) {
                    warn!(
                        installed = %name,
                        pkgbase = %e.pkgbase,
                        "AUR pkgbase provides this pkg but also conflicts with it without a replaces= — skipped as auto-upgrade; opt in with `aurox -S {}` to switch",
                        e.pkgbase,
                    );
                    continue;
                }
                e
            }
            AurClass::NotInAur => {
                if !name.is_makepkg_debug_split() {
                    warn!(%name, "foreign pkg not in AUR index");
                }
                continue;
            }
        };
        let is_vcs = entry.pkgbase.is_vcs();
        if is_vcs && !devel.rebuilds_vcs() {
            continue;
        }
        let aur_ver = entry.version();
        // Typed vercmp via `Ver::is_outdated` — `<` and `==` on `Ver` invoke
        // libalpm's vercmp under the hood.
        let need = (is_vcs && devel.rebuilds_vcs()) || installed_ver.is_outdated(&aur_ver);
        if need {
            out.push(PkgUpgrade {
                repo: invoke::REPO_AUR.into(),
                name,
                old_ver: installed_ver,
                new_ver: aur_ver,
            });
        }
    }
    out
}

/// Whether `entry` is a transparent upgrade target for an installed pkg
/// named `name` — i.e. a `pacman -U` of `entry`'s artifacts would not need
/// the user to consent to removing `name` first.
///
/// Rules, in order:
///
/// * Direct pkgname match: `name` IS one of the pkgbase's own pkgnames.
///   Always transparent — the entry literally is the installed pkg.
/// * Cross-rename via `provides=name`: transparent iff the entry does NOT
///   also declare `conflicts=name`. A `conflicts=name` says "I take over
///   this slot but you have to remove the old `name` first", and pacman's
///   "Remove `name`? [y/N]" question defaults to N under `--noconfirm`.
/// * `conflicts=name` paired with `replaces=name`: still transparent. The
///   `replaces=` flips pacman's default to yes, so the swap happens
///   automatically without a user prompt.
///
/// Real-world trigger (dotnet-runtime-7.0 → dotnet-core-7.0-bin): the AUR
/// pkgbase declares `provides=dotnet-runtime-7.0` AND
/// `conflicts=dotnet-runtime-7.0` AND no `replaces=`, so it's a switch the
/// user must opt into explicitly. Auto-queueing it from `-Qu`/`-Syu` just
/// produces a `failed to prepare transaction` every loop iteration.
fn is_transparent_upgrade_for(entry: &IndexEntry, name: &PkgName) -> bool {
    if entry.pkgnames.iter().any(|p| p.name == *name) {
        return true;
    }
    if !entry.conflicts.iter().any(|c| c.refers_to(name)) {
        return true;
    }
    entry.replaces.iter().any(|r| r.refers_to(name))
}

// `-debug` recognition is on `PkgName::is_makepkg_debug_split` — see
// `crate::names`. Tests live there too.

#[cfg(test)]
mod tests {
    //! Inline coverage for `aur_upgrades`. The integration paths
    //! (`-Qu`, `-Syu` picker) are pinned by container smokes 41 / 42;
    //! these unit tests cover the branches the smokes don't observe
    //! directly — the devel filter axis on a synthetic index, and the
    //! `NotInAur` skip on a debug-split pkgname.
    use super::*;
    use crate::index::schema::{IndexEntry, IndexFile, Pkgname};
    use crate::names::PkgName;
    use crate::version::Version;

    fn entry(pkgbase: &str, pkgname: &str, pkgver: &str, pkgrel: &str) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: vec![Pkgname {
                name: pkgname.into(),
                provides: Vec::new(),
                pkgdesc: None,
            }],
            pkgver: pkgver.into(),
            pkgrel: pkgrel.into(),
            ..IndexEntry::default()
        }
    }

    fn idx(entries: Vec<IndexEntry>) -> IndexFile {
        IndexFile {
            entries,
            ..IndexFile::empty()
        }
    }

    fn pac_with_foreign(installed: &[(&str, &str)]) -> PacmanIndex {
        let mut pac = PacmanIndex::default();
        for (name, ver) in installed {
            pac.installed
                .insert(PkgName::from(*name), Version::from(*ver));
        }
        pac
    }

    /// `-git` pkgbase installed at the same version the AUR ships:
    /// `is_outdated` is false (vercmp-equal), so the only path that can
    /// surface it is `devel && is_vcs`. Without `--devel` the row must
    /// be skipped silently.
    #[test]
    fn vcs_skipped_without_devel() {
        let i = idx(vec![entry("foo-git", "foo-git", "r1.deadbeef", "1")]);
        let s = Secondary::build(&i);
        let pac = pac_with_foreign(&[("foo-git", "r1.deadbeef-1")]);
        assert!(aur_upgrades(&i, &s, &pac, DevelPolicy::Skip).is_empty());
    }

    /// Same setup but with `--devel`: the row must appear even though
    /// vercmp says the installed and AUR versions are equal.
    #[test]
    fn vcs_forced_with_devel_even_when_vercmp_equal() {
        let i = idx(vec![entry("foo-git", "foo-git", "r1.deadbeef", "1")]);
        let s = Secondary::build(&i);
        let pac = pac_with_foreign(&[("foo-git", "r1.deadbeef-1")]);
        let out = aur_upgrades(&i, &s, &pac, DevelPolicy::Rebuild);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, PkgName::from("foo-git"));
        assert_eq!(out[0].repo, invoke::REPO_AUR);
    }

    /// Non-VCS upgrade: vercmp drives the inclusion regardless of the
    /// devel flag. Guards against a refactor that gated the
    /// `is_outdated` arm on `is_vcs`.
    #[test]
    fn non_vcs_outdated_listed_both_modes() {
        let i = idx(vec![entry("foo", "foo", "2.0", "1")]);
        let s = Secondary::build(&i);
        let pac = pac_with_foreign(&[("foo", "1.0-1")]);
        for devel in [DevelPolicy::Skip, DevelPolicy::Rebuild] {
            let out = aur_upgrades(&i, &s, &pac, devel);
            assert_eq!(out.len(), 1, "devel={devel:?}: expected one upgrade row");
            assert_eq!(out[0].name, PkgName::from("foo"));
            assert_eq!(out[0].old_ver, Version::from("1.0-1"));
            assert_eq!(out[0].new_ver, Version::from("2.0-1"));
        }
    }

    /// A foreign pkg whose name resolves to `NotInAur` doesn't crash
    /// or produce a row. Includes a debug-split name to pin the silent
    /// path (no panic when the warn-suppression branch is taken).
    #[test]
    fn foreign_not_in_aur_skipped() {
        let i = idx(vec![]);
        let s = Secondary::build(&i);
        let pac = pac_with_foreign(&[("foo-debug", "1.0-1"), ("orphan-pkg", "1.0-1")]);
        assert!(aur_upgrades(&i, &s, &pac, DevelPolicy::Skip).is_empty());
        assert!(aur_upgrades(&i, &s, &pac, DevelPolicy::Rebuild).is_empty());
    }

    /// `is_transparent_upgrade_for` coverage. The principle: a pkgbase is
    /// transparent when (a) it owns the installed pkgname directly, OR
    /// (b) it provides the pkgname without conflicting with it, OR (c)
    /// it conflicts AND replaces (pacman auto-handles the swap).
    use crate::names::PkgTarget;

    fn entry_with(
        pkgbase: &str,
        provides: &[&str],
        conflicts: &[&str],
        replaces: &[&str],
    ) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: vec![Pkgname {
                name: pkgbase.into(),
                provides: Vec::new(),
                pkgdesc: None,
            }],
            provides: provides.iter().map(|s| PkgTarget::new(*s)).collect(),
            conflicts: conflicts.iter().map(|s| PkgTarget::new(*s)).collect(),
            replaces: replaces.iter().map(|s| PkgTarget::new(*s)).collect(),
            pkgver: "1".into(),
            pkgrel: "1".into(),
            ..IndexEntry::default()
        }
    }

    #[test]
    fn direct_pkgname_match_is_transparent() {
        // Even with a (nonsensical) self-conflict declared, the direct
        // pkgname match wins — the entry literally IS `foo`.
        let e = entry_with("foo", &[], &["foo"], &[]);
        assert!(is_transparent_upgrade_for(&e, &PkgName::from("foo")));
    }

    #[test]
    fn provides_only_is_transparent() {
        let e = entry_with("new-foo", &["foo"], &[], &[]);
        assert!(is_transparent_upgrade_for(&e, &PkgName::from("foo")));
    }

    /// The dotnet-runtime-7.0 case: provides + conflicts, NO replaces.
    /// Pacman's "Remove foo?" defaults to N under --noconfirm.
    #[test]
    fn provides_with_conflicts_without_replaces_is_not_transparent() {
        let e = entry_with("new-foo-bin", &["foo"], &["foo"], &[]);
        assert!(!is_transparent_upgrade_for(&e, &PkgName::from("foo")));
    }

    /// A renaming migration: provides + conflicts + replaces. Pacman
    /// auto-replaces with no prompt — transparent.
    #[test]
    fn provides_with_conflicts_and_replaces_is_transparent() {
        let e = entry_with("new-foo", &["foo"], &["foo"], &["foo"]);
        assert!(is_transparent_upgrade_for(&e, &PkgName::from("foo")));
    }

    /// Version constraints in conflicts/replaces are stripped via
    /// `PkgTarget::bare()` before the name compare.
    #[test]
    fn version_constraints_in_conflicts_are_stripped() {
        let e = entry_with("new-foo-bin", &["foo"], &["foo>=1.0"], &[]);
        assert!(!is_transparent_upgrade_for(&e, &PkgName::from("foo")));
        let e2 = entry_with("new-foo", &["foo"], &["foo>=1.0"], &["foo<2.0"]);
        assert!(is_transparent_upgrade_for(&e2, &PkgName::from("foo")));
    }

    /// Integration: a foreign pkg whose only AUR match is a conflicting
    /// pkgbase (provides + conflicts, no replaces) is NOT queued for
    /// upgrade. Regression target for the dotnet-runtime-7.0 →
    /// dotnet-core-7.0-bin trap that produced `failed to prepare
    /// transaction` every loop iteration.
    #[test]
    fn provides_with_conflict_skipped_in_aur_upgrades() {
        let e = entry_with("new-foo-bin", &["foo"], &["foo"], &[]);
        let i = idx(vec![IndexEntry {
            pkgver: "2".into(),
            ..e
        }]);
        let s = Secondary::build(&i);
        let pac = pac_with_foreign(&[("foo", "1-1")]);
        let out = aur_upgrades(&i, &s, &pac, DevelPolicy::Skip);
        assert!(
            out.is_empty(),
            "conflicting pkgbase should not auto-upgrade `foo`; got: {out:?}",
        );
    }

    /// Integration: a provides-only match still produces an upgrade row.
    /// Guards the existing dotnet-rename path against over-aggressive
    /// filtering.
    #[test]
    fn provides_only_still_produces_aur_upgrade() {
        let e = entry_with("new-foo", &["foo"], &[], &[]);
        let i = idx(vec![IndexEntry {
            pkgver: "2".into(),
            ..e
        }]);
        let s = Secondary::build(&i);
        let pac = pac_with_foreign(&[("foo", "1-1")]);
        let out = aur_upgrades(&i, &s, &pac, DevelPolicy::Skip);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, PkgName::from("foo"));
    }
}
