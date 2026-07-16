//! Plan and final-summary presentation helpers for the build pipeline.
//!
//! Kept separate from the pipeline itself so the orchestration in
//! `super` reads as decisions + state mutation, not ui formatting.

use crate::index::{IndexEntry, IndexFile};
use crate::names::PkgBase;
use crate::pacman::alpm_db::PacmanIndex;
use crate::resolver::Plan;
use crate::ui;
use std::collections::BTreeMap;

use super::{BuildFailure, RunReport};

/// Render the resolved [`Plan`] to stderr as aligned `name  version` tables
/// — one group per source — mirroring the style of [`ui::upgrade_table`] used
/// by `-Su`. Versions are looked up live from `pac` (sync DBs) and `idx`
/// (AUR index), so the plan answers "which exact version would land?" for
/// every row before the user confirms.
pub(super) fn plan(plan: &Plan, idx: &IndexFile, pac: &PacmanIndex) {
    // Both dep buckets disclose here — the union covers deps aurox installs
    // itself (`transitive_repo`) and deps pacman resolves natively within a
    // repo target's transaction (`disclosed_repo_deps`).
    let repo_deps = plan.repo_dep_disclosure();
    if plan.direct_repo.is_empty() && repo_deps.is_empty() && plan.aur_strata.is_empty() {
        ui::info("plan: nothing to do");
        return;
    }
    if !plan.direct_repo.is_empty() {
        ui::install_table(
            "Repo packages (explicit)",
            &rows_for_repo(&plan.direct_repo, pac),
        );
    }
    if !repo_deps.is_empty() {
        ui::install_table("Repo dependencies", &rows_for_repo(&repo_deps, pac));
    }
    if !plan.aur_strata.is_empty() {
        let total = plan.aur_strata.len();
        if total == 1 {
            ui::install_table("AUR build order", &rows_for_aur(&plan.aur_strata[0], idx));
        } else {
            for (i, stratum) in plan.aur_strata.iter().enumerate() {
                ui::install_table(
                    &format!("AUR build stratum {}/{total}", i + 1),
                    &rows_for_aur(stratum, idx),
                );
            }
        }
    }
}

/// Pair each repo pkgname with its sync-repo version. A name that only
/// matched via a virtual `provides` won't carry a version of its own (pacman
/// will choose a concrete provider at install time); render an empty version
/// cell rather than guessing.
fn rows_for_repo(names: &[String], pac: &PacmanIndex) -> Vec<(String, String)> {
    names
        .iter()
        .map(|n| {
            // `sync_version` returns `Option<&Ver>` post-Phase B; `Ver::as_str`
            // is the explicit text-rendering boundary for the table cell.
            let ver = pac
                .sync_version(n)
                .map(|v| v.as_str().to_owned())
                .unwrap_or_default();
            (n.clone(), ver)
        })
        .collect()
}

/// Pair each AUR pkgbase with its index version (`[epoch:]pkgver-pkgrel`).
/// All pkgnames in a split pkgbase share that version, so the pkgbase row
/// is unambiguous even when only a subset of pkgnames will be installed.
fn rows_for_aur(pkgbases: &[PkgBase], idx: &IndexFile) -> Vec<(String, String)> {
    pkgbases
        .iter()
        .map(|pb| {
            // Surrender the typed `Version` to a `String` at the table-cell
            // boundary via `into_inner` — not via `Display`/`to_string`.
            let ver = idx
                .entries
                .iter()
                .find(|e| e.pkgbase == *pb)
                .map(IndexEntry::version)
                .unwrap_or_default()
                .into_inner();
            (pb.to_string(), ver)
        })
        .collect()
}

/// Print a per-pkgbase outcome summary at the end of a multi-pkgbase run.
/// Skips itself for the trivial single-pkgbase happy path where the failure
/// message above already says everything.
pub(super) fn final_summary(report: &RunReport) {
    let total = report.installed.len()
        + report.failed.len()
        + report.skipped_user.len()
        + report.skipped_dep.len()
        + report.interrupted.len();
    if total < 2 {
        return;
    }
    ui::info("build summary");
    if !report.installed.is_empty() {
        // `Vec<PkgBase>::join` uses `Borrow<str>` to concatenate via the
        // underlying strings — no per-element conversion needed.
        ui::note(&format!(
            "installed ({}): {}",
            report.installed.len(),
            report.installed.join(" ")
        ));
    }
    for pb in &report.skipped_user {
        ui::note(&format!("skipped {pb} (user)"));
    }
    for pb in &report.interrupted {
        ui::warn(&format!("interrupted {pb}"));
    }
    let dep_sorted: BTreeMap<&PkgBase, &PkgBase> = report.skipped_dep.iter().collect();
    for (pb, blocker) in dep_sorted {
        ui::warn(&format!("skipped {pb} (blocked by {blocker})"));
    }
    let failed_sorted: BTreeMap<&PkgBase, &BuildFailure> = report.failed.iter().collect();
    for (pb, fail) in failed_sorted {
        ui::error(&format!(
            "failed {pb} ({}): {}",
            fail.phase(),
            fail.detail()
        ));
    }
}
