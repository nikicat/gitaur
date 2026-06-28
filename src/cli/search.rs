//! `gaur <term>...` — yay-style fuzzy search → multi-select → install.
//!
//! Wired up from [`crate::cli::dispatch`] for the no-operation-letter case.
//! Shows sync-repo packages alongside AUR ones (like yay/paru): repo hits
//! first in pacman.conf precedence order, then AUR hits sorted freshest-commit
//! first. Picked rows are routed through [`crate::build::cmd_install`], which
//! re-classifies each name (pacman wins over AUR) and installs accordingly.

use crate::build::{self, Target};
use crate::cli::Cli;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::{self, IndexEntry, secondary::Secondary};
use crate::names::{PkgTarget, SearchTerm};
use crate::pacman::alpm_db::{self, RepoHit};
use crate::pacman::invoke::REPO_AUR;
use crate::paths;
use crate::runopts;
use crate::ui;

use console::style;
use dialoguer::MultiSelect;
use std::io::IsTerminal;
use tracing::{debug, info, instrument};

/// One search hit — either a sync-repo package or an AUR pkgbase.
///
/// Borrows the AUR entry from the loaded index; repo hits are owned (their
/// `Alpm` handle is already dropped by the time we build rows). `pub(crate)` so
/// the interactive shell ([`crate::cli::shell`]) reuses the same row model +
/// labels for its numbered result list.
pub(crate) enum Row<'a> {
    Repo(RepoHit),
    Aur(&'a IndexEntry),
}

impl Row<'_> {
    /// The name to install if this row is picked, widened to the unclassified
    /// [`PkgTarget`] that the picker domain deals in (a repo pkgname or an AUR
    /// pkgbase — only the resolver tells them apart). Uses the existing
    /// `From<&PkgName>` / `From<&PkgBase>` widening conversions, so this is the
    /// only place the two row kinds collapse into one type, and there's no
    /// second dispatch downstream.
    pub(crate) fn picked(&self) -> PkgTarget {
        match self {
            Row::Repo(r) => PkgTarget::from(&r.name),
            Row::Aur(e) => PkgTarget::from(&e.pkgbase),
        }
    }

    /// The repo bucket this row belongs to (`core`, `extra`, …, or `aur`), for
    /// the shell's repo-filter selectors (`add extra`).
    pub(crate) const fn repo_name(&self) -> &str {
        match self {
            Row::Repo(r) => r.repo.as_str(),
            Row::Aur(_) => REPO_AUR,
        }
    }

    /// The display label for this row (no leading number/checkbox), colored
    /// per `color`. The plain form is also what dialoguer measures for width.
    pub(crate) fn label(&self, color: bool) -> String {
        if color {
            label_colored(self)
        } else {
            label_plain(self)
        }
    }
}

/// Outcome of the picker step — distinguishes the three terminal states the
/// caller must dispatch on differently:
///   * `Listed` — non-interactive (no TTY or `--noconfirm`); the search hits
///     were printed to stdout, nothing to install. The caller returns `Ok(0)`
///     so `gaur foo | head` is a legitimate "search" pipeline.
///   * `Picked` — interactive: the user kept at least one row. Caller routes
///     into `build::cmd_install`.
///   * `Aborted` — interactive: the user explicitly cleared every row. Caller
///     returns `Error::UserAbort` so scripts can detect the abort.
enum PickOutcome {
    Listed,
    Picked(Vec<PkgTarget>),
    Aborted,
}

/// Entry point for the bare-positional shortcut.
///
/// `terms` are the freeform regex fragments the user typed; they're combined
/// as an AND filter (same semantics as `-Ss`). Sync-repo and AUR matches land
/// in a single picker so the user can pick across both sources in one pass.
#[instrument(skip(cfg))]
pub fn cmd_search_install(cfg: &Config, cli: &Cli, terms: &[SearchTerm]) -> Result<u8> {
    let noconfirm = cli.noconfirm;
    let asdeps = cli.asdeps;

    let regexes: Vec<regex::Regex> = terms
        .iter()
        .map(SearchTerm::compile)
        .collect::<std::result::Result<_, _>>()?;

    // Repo + AUR searches are independent I/O — an alpm DB scan vs an index
    // mmap. Run them concurrently and merge below.
    let (repo_res, aur_res) = rayon::join(
        || alpm_db::search_sync(terms),
        // `propagate` so `load_or_resync` sees `--noresync` even when rayon
        // runs this closure on a worker thread (its RunOpts TLS is otherwise
        // the default).
        runopts::propagate(|| -> Result<Option<index::IndexFile>> {
            let path = paths::index_path();
            if !path.exists() {
                return Ok(None);
            }
            Ok(Some(index::load_or_resync(cfg, &path)?))
        }),
    );
    let repo_hits = repo_res?;
    let idx = aur_res?;
    if idx.is_none() {
        ui::warn("no AUR index; showing repo matches only (run `gaur -Sy` to index the AUR)");
    }

    let aur_hits: Vec<&IndexEntry> = match idx.as_ref() {
        Some(idx) => {
            let by = Secondary::build(idx);
            let mut hits = by.search(idx, &regexes);
            // Freshest commit first; tie-break on pkgbase so equal timestamps
            // (common in fixtures, possible in the wild) stay deterministic.
            hits.sort_by(|a, b| {
                b.commit_time_unix
                    .cmp(&a.commit_time_unix)
                    .then_with(|| a.pkgbase.cmp(&b.pkgbase))
            });
            hits
        }
        None => Vec::new(),
    };
    info!(
        repo = repo_hits.len(),
        aur = aur_hits.len(),
        "search results"
    );

    if repo_hits.is_empty() && aur_hits.is_empty() {
        ui::info(&format!(
            "no packages match `{}`",
            terms
                .iter()
                .map(SearchTerm::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        ));
        return Ok(0);
    }

    // Repo rows first (yay/paru "official repos on top"), AUR rows after in
    // freshest-first order.
    let rows: Vec<Row<'_>> = repo_hits
        .into_iter()
        .map(Row::Repo)
        .chain(aur_hits.into_iter().map(Row::Aur))
        .collect();

    match pick(&rows, noconfirm)? {
        PickOutcome::Listed => Ok(0),
        PickOutcome::Aborted => Err(Error::UserAbort),
        PickOutcome::Picked(selected) => {
            debug!(picked = selected.len(), "search-install selection");
            // `Target.spec` is the freeform argv-shaped string, so
            // `into_inner()` is the sanctioned name→String boundary here.
            let targets: Vec<Target> = selected
                .into_iter()
                .map(|t| Target::bare(t.into_inner()))
                .collect();
            build::cmd_install(cfg, &targets, noconfirm, asdeps, false)
        }
    }
}

/// Render the picker (or, when non-interactive, dump labels to stdout and
/// install nothing — auto-installing every regex hit is too dangerous to do
/// without a human in the loop; the user can re-run interactively or with
/// `-S <pkg>` once they know the exact pkgname).
fn pick(rows: &[Row<'_>], noconfirm: bool) -> Result<PickOutcome> {
    let labels_plain: Vec<String> = rows.iter().map(label_plain).collect();

    let interactive = !noconfirm && std::io::stdin().is_terminal();
    if !interactive {
        // Pipelines (`gaur foo | grep …`) and `--noconfirm` callers both
        // land here. We print the matches so the search itself is useful and
        // exit cleanly so the shell doesn't treat the listing as a failure.
        for l in &labels_plain {
            println!("{l}");
        }
        return Ok(PickOutcome::Listed);
    }

    let labels_colored: Vec<String>;
    let labels_display: &[String] = if ui::color_on() {
        labels_colored = rows.iter().map(label_colored).collect();
        &labels_colored
    } else {
        &labels_plain
    };

    let chosen = MultiSelect::new()
        .with_prompt("Select packages to install (space toggles, enter confirms)")
        .items(labels_display)
        // Same rationale as the upgrade picker: dialoguer would otherwise
        // re-list every selected row as a single wrapped line that duplicates
        // the picker output. We print our own short summary instead.
        .report(false)
        .interact()
        .map_err(|e| Error::other(format!("search picker: {e}")))?;

    if chosen.is_empty() {
        return Ok(PickOutcome::Aborted);
    }
    Ok(PickOutcome::Picked(
        chosen.into_iter().map(|i| rows[i].picked()).collect(),
    ))
}

/// One picker row, plain ASCII — fed to dialoguer for width math.
fn label_plain(row: &Row<'_>) -> String {
    match row {
        Row::Repo(r) => {
            let installed = if r.installed { " [installed]" } else { "" };
            match r.desc.as_deref() {
                Some(d) if !d.is_empty() => {
                    format!("{}/{} {}{installed}  {d}", r.repo, r.name, r.version)
                }
                _ => format!("{}/{} {}{installed}", r.repo, r.name, r.version),
            }
        }
        Row::Aur(e) => {
            let ver = aur_version(e);
            match e.display_desc() {
                Some(d) => format!("aur/{} {ver}  {d}", e.pkgbase),
                None => format!("aur/{} {ver}", e.pkgbase),
            }
        }
    }
}

/// Colored variant of [`label_plain`] — matches `-Ss` / install-table styling
/// (repo prefix bold, version green, description dimmed, installed marker cyan).
fn label_colored(row: &Row<'_>) -> String {
    match row {
        Row::Repo(r) => {
            let mut head = format!(
                "{}/{} {}",
                ui::repo(&r.repo),
                style(&r.name).bold(),
                style(&r.version).green(),
            );
            if r.installed {
                head = format!("{head} {}", style("[installed]").cyan());
            }
            match r.desc.as_deref() {
                Some(d) if !d.is_empty() => format!("{head}  {}", ui::dim(d)),
                _ => head,
            }
        }
        Row::Aur(e) => {
            let ver = aur_version(e);
            let head = format!(
                "{}/{} {}",
                ui::repo("aur"),
                style(&e.pkgbase).bold(),
                style(ver).green(),
            );
            match e.display_desc() {
                Some(d) => format!("{head}  {}", ui::dim(d)),
                None => head,
            }
        }
    }
}

fn aur_version(e: &IndexEntry) -> String {
    match e.epoch.as_deref() {
        Some(ep) if !ep.is_empty() => format!("{ep}:{}-{}", e.pkgver, e.pkgrel),
        _ => format!("{}-{}", e.pkgver, e.pkgrel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::index::schema::Pkgname;
    use crate::names::PkgName;
    use crate::version::Version;

    fn mk(pkgbase: &str, desc: Option<&str>, epoch: Option<&str>) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: vec![Pkgname {
                name: pkgbase.into(),
                provides: Vec::new(),
                pkgdesc: None,
            }],
            pkgver: "1.2.3".into(),
            pkgrel: "4".into(),
            epoch: epoch.map(str::to_owned),
            pkgdesc: desc.map(str::to_owned),
            ..Default::default()
        }
    }

    fn repo(name: &str, desc: Option<&str>, installed: bool) -> RepoHit {
        RepoHit {
            repo: "extra".into(),
            name: PkgName::new(name),
            version: Version::from("2.0-1"),
            desc: desc.map(str::to_owned),
            installed,
        }
    }

    /// `label_plain` is the byte-exact string dialoguer measures for wrap
    /// width — must stay free of ANSI escapes, must surface pkgbase / version
    /// / description so the user has enough to pick from.
    #[test]
    fn aur_label_plain_no_ansi_and_has_all_pieces() {
        let l = label_plain(&Row::Aur(&mk("foo", Some("does foo"), None)));
        assert!(!l.contains('\u{1b}'), "ANSI leaked into plain label: {l:?}");
        assert_eq!(l, "aur/foo 1.2.3-4  does foo");
    }

    #[test]
    fn aur_label_plain_drops_empty_or_missing_description() {
        assert_eq!(
            label_plain(&Row::Aur(&mk("bar", None, None))),
            "aur/bar 1.2.3-4"
        );
        assert_eq!(
            label_plain(&Row::Aur(&mk("baz", Some(""), None))),
            "aur/baz 1.2.3-4"
        );
    }

    #[test]
    fn aur_label_plain_includes_epoch_when_set() {
        let l = label_plain(&Row::Aur(&mk("qux", None, Some("2"))));
        assert_eq!(l, "aur/qux 2:1.2.3-4");
    }

    #[test]
    fn aur_label_plain_skips_empty_epoch_string() {
        let l = label_plain(&Row::Aur(&mk("qux", None, Some(""))));
        assert!(l.starts_with("aur/qux 1.2.3-4"), "got: {l:?}");
    }

    /// Repo rows render in pacman `repo/name version` shape, with the
    /// `[installed]` marker only when the user already has the package.
    #[test]
    fn repo_label_plain_shape_and_installed_marker() {
        assert_eq!(
            label_plain(&Row::Repo(repo("firefox", Some("a browser"), false))),
            "extra/firefox 2.0-1  a browser"
        );
        assert_eq!(
            label_plain(&Row::Repo(repo("vim", None, true))),
            "extra/vim 2.0-1 [installed]"
        );
    }

    /// Both row kinds widen to the unclassified `PkgTarget` the install path
    /// consumes — repo rows from their pkgname, AUR rows from their pkgbase.
    /// The resolver (not the picker) re-classifies, so the picker only has to
    /// hand over the name string in the right type.
    #[test]
    fn picked_widens_repo_pkgname_and_aur_pkgbase() {
        assert_eq!(
            Row::Repo(repo("firefox", None, false)).picked(),
            PkgTarget::from("firefox")
        );
        let e = mk("bisq", None, None);
        assert_eq!(Row::Aur(&e).picked(), PkgTarget::from("bisq"));
    }

    /// Freshest-commit-first ordering with a deterministic pkgbase tie-break.
    /// Mirrors the sort in `cmd_search_install` so a refactor that drops the
    /// `commit_time_unix` key (reverting to alphabetical) fails here.
    #[test]
    fn aur_hits_sort_by_commit_time_desc_then_pkgbase() {
        let mut a = mk("alpha", None, None);
        a.commit_time_unix = 100;
        let mut b = mk("bravo", None, None);
        b.commit_time_unix = 300;
        let mut c = mk("charlie", None, None);
        c.commit_time_unix = 300; // ties with bravo → pkgbase order
        let mut hits = [&a, &b, &c];
        hits.sort_by(|x, y| {
            y.commit_time_unix
                .cmp(&x.commit_time_unix)
                .then_with(|| x.pkgbase.cmp(&y.pkgbase))
        });
        assert_eq!(hits[0].pkgbase, "bravo");
        assert_eq!(hits[1].pkgbase, "charlie");
        assert_eq!(hits[2].pkgbase, "alpha");
    }
}
