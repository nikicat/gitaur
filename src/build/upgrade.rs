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
