//! `gitaur -Qu` and the shared upgrade-query plumbing that also feeds the
//! `-Syu` interactive picker. Read-only: walks alpm + the AUR index file,
//! never shells out to `pacman -S` or asks for sudo.

use crate::error::Result;
use crate::index::secondary::Secondary;
use crate::index::{self, IndexFile};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke::{self, PkgUpgrade};
use crate::paths;
use crate::ui;
use tracing::{instrument, warn};

/// `gitaur -Qu` — show the union of pacman-repo and AUR upgrade candidates
/// in one flat, severity-sorted table grouped by `repo` column. Read-only
/// and unprivileged (no sudo), so safe to call both as the bare `-Qu` and
/// as a preview before `-Syu` runs.
#[instrument]
pub fn cmd_query_upgrades(devel: bool) -> Result<u8> {
    ui::upgrade_table(&collect_upgrade_plan(devel)?);
    Ok(0)
}

/// Gather the merged repo + AUR upgrade list. Shared by `-Qu` (read-only
/// rendering) and `-Syu` (feeds the interactive picker). Unprivileged —
/// reads alpm and the AUR index file only.
pub fn collect_upgrade_plan(devel: bool) -> Result<Vec<PkgUpgrade>> {
    let mut plan = invoke::query_repo_upgrades()?;
    let idx_path = paths::index_path();
    if idx_path.exists() {
        let idx = index::load(&idx_path)?;
        let by = Secondary::build(&idx);
        let alpm = alpm_db::open()?;
        let pac = PacmanIndex::build(&alpm);
        plan.extend(aur_upgrades(&idx, &by, &pac, devel));
    }
    Ok(plan)
}

/// Scan the localdb for foreign pkgs with a newer version in the AUR index.
///
/// `devel=true` forces every VCS pkgbase (`-git`/`-svn`/`-hg`/`-bzr`) into
/// the list regardless of vercmp, since their `pkgver` is only refreshed by
/// `makepkg`. Otherwise VCS pkgs are skipped (their on-disk version always
/// looks stale).
fn aur_upgrades(
    idx: &IndexFile,
    by: &Secondary,
    pac: &PacmanIndex,
    devel: bool,
) -> Vec<PkgUpgrade> {
    let mut out = Vec::new();
    for (name, installed_ver) in pac.foreign() {
        // Cross-domain classifier: pacman has `name` as a localdb pkgname;
        // we ask AUR how it relates — own pkgname / provides / pkgbase /
        // unknown. The provides arm is what surfaces dotnet-style
        // renames (foreign `dotnet-runtime-7.0` matched by an AUR pkg's
        // `provides=` declaration).
        use crate::index::secondary::AurClass;
        let entry = match by.classify_foreign(idx, &name) {
            AurClass::AsPkgname(e) | AurClass::AsProvides(e) | AurClass::AsPkgbase(e) => e,
            AurClass::NotInAur => {
                if !name.is_makepkg_debug_split() {
                    warn!(%name, "foreign pkg not in AUR index");
                }
                continue;
            }
        };
        let is_vcs = entry.pkgbase.is_vcs();
        if !devel && is_vcs {
            continue;
        }
        let aur_ver = entry.version();
        // Typed vercmp via `Ver::is_outdated` — `<` and `==` on `Ver` invoke
        // libalpm's vercmp under the hood.
        let need = (devel && is_vcs) || installed_ver.is_outdated(&aur_ver);
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
        assert!(aur_upgrades(&i, &s, &pac, false).is_empty());
    }

    /// Same setup but with `--devel`: the row must appear even though
    /// vercmp says the installed and AUR versions are equal.
    #[test]
    fn vcs_forced_with_devel_even_when_vercmp_equal() {
        let i = idx(vec![entry("foo-git", "foo-git", "r1.deadbeef", "1")]);
        let s = Secondary::build(&i);
        let pac = pac_with_foreign(&[("foo-git", "r1.deadbeef-1")]);
        let out = aur_upgrades(&i, &s, &pac, true);
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
        for devel in [false, true] {
            let out = aur_upgrades(&i, &s, &pac, devel);
            assert_eq!(out.len(), 1, "devel={devel}: expected one upgrade row");
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
        assert!(aur_upgrades(&i, &s, &pac, false).is_empty());
        assert!(aur_upgrades(&i, &s, &pac, true).is_empty());
    }
}
