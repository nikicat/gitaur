//! Package search across the sync repos + the AUR — one merged, ranked list.
//!
//! Two non-interactive surfaces live here, both wired up from
//! [`crate::cli::dispatch`]: [`cmd_search`] is `-Ss` (pacman's plain
//! `repo/name version` output), and [`cmd_search_install`] is the bare
//! `aurox <term>...` shortcut in a pipe (the aligned [`ui::search_table`]).
//! Interactively the bare shortcut launches the shell REPL seeded with the
//! search instead (see [`crate::cli::shell`]) — there is no picker; the REPL
//! is the one interactive surface. The [`Row`] model, ranking, and the
//! [`ui::search_table`] renderer are shared with the shell so every surface
//! ranks and renders matches identically.

use crate::config::Config;
use crate::context;
use crate::error::Result;
use crate::index::{self, AurIndexData, IndexEntry};
use crate::names::{NameMatch, PkgName, PkgTarget, RepoName, SearchTerm};
use crate::pacman::alpm_db::{self, PacmanIndex, RepoHit};
use crate::pacman::invoke::REPO_AUR;
use crate::ui;
use crate::units::UnixTime;
use crate::version::Version;

use std::cmp::Ordering;
use tracing::{info, instrument};

/// One search hit — either a sync-repo package or an AUR pkgbase.
///
/// Borrows the AUR entry from the loaded index; repo hits are owned (their
/// `Alpm` handle is already dropped by the time we build rows). `pub(crate)` so
/// the interactive shell ([`crate::cli::shell`]) reuses the same row model +
/// ranking + [`search_row`] conversion for its numbered result table.
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

    /// The installed package this row answers for: the repo package itself
    /// (state already carried on the hit), or the first of an AUR pkgbase's
    /// split pkgnames present in the local DB — checking the pkgbase name
    /// alone would miss a split member (installed `linux-tkg-bmq` under
    /// pkgbase `linux-tkg`). Drives the `[installed]` marker and the
    /// `old → new` version diff in both `-Ss` and the shell table.
    pub(crate) fn installed_name(&self, pac: &PacmanIndex) -> Option<&PkgName> {
        match self {
            Row::Repo(r) => r.installed.then_some(&r.name),
            Row::Aur(e) => e
                .pkgnames
                .iter()
                .map(|p| &p.name)
                .find(|n| pac.is_installed(n)),
        }
    }

    /// The repo bucket this row belongs to (`core`, `extra`, …, or `aur`), for
    /// the shell's repo-filter selectors (`add extra`).
    pub(crate) fn repo_name(&self) -> &str {
        match self {
            Row::Repo(r) => r.repo.as_str(),
            Row::Aur(_) => REPO_AUR,
        }
    }

    /// The version a pick would install — the repo package's, or the AUR
    /// entry's combined `[epoch:]pkgver-pkgrel`.
    fn version(&self) -> Version {
        match self {
            Row::Repo(r) => r.version.clone(),
            Row::Aur(e) => e.version(),
        }
    }

    /// The row's one-line description, if its source carries one.
    fn desc(&self) -> Option<String> {
        match self {
            Row::Repo(r) => r.desc.clone(),
            Row::Aur(e) => e.display_desc().map(str::to_owned),
        }
    }
}

/// Compile the user's freeform terms into the regexes ranking and matching
/// consume — the shared AND-filter semantics of every search surface.
fn compile_terms(terms: &[SearchTerm]) -> Result<Vec<regex::Regex>> {
    Ok(terms
        .iter()
        .map(SearchTerm::compile)
        .collect::<std::result::Result<_, _>>()?)
}

/// Query both providers: sync-repo hits for `terms`, plus the loaded AUR data.
///
/// The two are independent I/O — an alpm DB scan vs an index mmap — so they
/// run concurrently. The AUR side loads *empty* when not in play (see
/// [`AurIndexData::load`]), so callers merge uniformly either way; the one
/// wording concession is a single nudge when the AUR is enabled but not yet
/// synced. Pacman-only mode is a standing choice — repo-only results need no
/// nudge.
fn gather(cfg: &Config, terms: &[SearchTerm]) -> Result<(Vec<RepoHit>, AurIndexData)> {
    let (repo_res, aur_res) = context::join(
        || alpm_db::search_sync(terms),
        // `context::join` propagates the caller's context so `load_or_resync`
        // sees `--noresync` and the right `state_dir()` even on the stolen
        // worker thread.
        || AurIndexData::load(cfg),
    );
    if index::AurState::probe(cfg) == index::AurState::NotSetUp {
        ui::warn("no AUR index; showing repo matches only (run `aurox -Sy` to index the AUR)");
    }
    Ok((repo_res?, aur_res?))
}

/// Merge repo and AUR hits into one relevance-ranked list (unlike yay's fixed
/// "repos on top", [`rank_rows`] interleaves both sources by match quality).
fn merged_rows<'a>(
    repo_hits: Vec<RepoHit>,
    aur_hits: Vec<&'a IndexEntry>,
    regexes: &[regex::Regex],
) -> Vec<Row<'a>> {
    let mut rows: Vec<Row<'a>> = repo_hits
        .into_iter()
        .map(Row::Repo)
        .chain(aur_hits.into_iter().map(Row::Aur))
        .collect();
    rank_rows(&mut rows, regexes);
    info!(rows = rows.len(), "search results");
    rows
}

/// `-Ss <regex>...` — search the sync repos and the AUR in one ranked list,
/// printed in pacman's plain `repo/name version` format (see
/// [`write_search_result`]).
///
/// Pacman-parity exit codes: 0 when at least one package matched, 1 when
/// none did (silently, like `pacman -Ss`) — so scripts can test for a hit.
#[instrument(skip(cfg))]
pub fn cmd_search(cfg: &Config, terms: &[SearchTerm]) -> Result<u8> {
    let regexes = compile_terms(terms)?;
    let (repo_hits, aur_data) = gather(cfg, terms)?;
    let rows = merged_rows(repo_hits, aur_data.search(&regexes), &regexes);
    if rows.is_empty() {
        return Ok(1);
    }
    let pac = PacmanIndex::build(&alpm_db::open()?);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for row in &rows {
        let install = ui::InstallState::from_installed(row.installed_name(&pac).is_some());
        write_search_result(&mut out, row, install)?;
    }
    Ok(0)
}

/// Render one search hit in pacman's `-Ss` format to `out`: the
/// `repo/name version` headline (` [installed]` appended pacman-style) and
/// the indented description line, omitted when the source has none.
///
/// Stdout (not stderr) so `aurox -Ss foo | head` works — the equivalent
/// `pacman -Ss` also writes results to stdout. A writer so the exact byte
/// layout is testable without spawning a process.
fn write_search_result<W: std::io::Write>(
    out: &mut W,
    row: &Row<'_>,
    install: ui::InstallState,
) -> std::io::Result<()> {
    let marker = match install {
        ui::InstallState::Installed => " [installed]",
        ui::InstallState::NotInstalled => "",
    };
    writeln!(
        out,
        "{}/{} {}{marker}",
        row.repo_name(),
        row.picked(),
        row.version()
    )?;
    if let Some(desc) = row.desc() {
        writeln!(out, "    {desc}")?;
    }
    Ok(())
}

/// Entry point for the bare-positional shortcut in a **non-interactive** run
/// (a pipe, or `--noconfirm`).
///
/// The interactive case never reaches here — [`crate::cli::dispatch`] launches
/// the shell REPL seeded with the search instead, so there is no picker (the
/// REPL is the one interactive surface).
///
/// `terms` are the freeform regex fragments the user typed, combined as an AND
/// filter (same semantics as `-Ss`). Sync-repo and AUR matches are merged into
/// one relevance-ranked list ([`rank_rows`]) and printed best-first — so
/// `aurox foo | head` surfaces the strongest hits. Nothing is installed:
/// auto-installing every regex hit is too dangerous without a human in the loop.
#[instrument(skip(cfg))]
pub fn cmd_search_install(cfg: &Config, terms: &[SearchTerm]) -> Result<u8> {
    let regexes = compile_terms(terms)?;
    let (repo_hits, aur_data) = gather(cfg, terms)?;
    let rows = merged_rows(repo_hits, aur_data.search(&regexes), &regexes);

    if rows.is_empty() {
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

    // Render the aligned table (installed emphasis + version diff + size), the
    // same machinery the shell uses. Build-time is left off the pipe listing
    // (empty metrics — no store lookups), so installed AUR rows show `~?` there.
    let pac = PacmanIndex::build(&alpm_db::open()?);
    let search_rows: Vec<ui::SearchRow> = rows.iter().map(|r| search_row(r, &pac)).collect();
    let table = ui::search_table(
        &search_rows,
        &pac,
        &ui::PreviewMetrics::empty(),
        ui::RowNumbers::Plain,
        ui::Paint::detect(),
    );
    for line in table.lines() {
        println!("{line}");
    }
    Ok(0)
}

/// Resolve one ranked [`Row`] into a [`ui::SearchRow`] for the aligned table:
/// its display name, available version, description, and — against `pac` — its
/// installed state and (when an upgrade is available) the installed version for
/// the `old → new` diff.
pub(crate) fn search_row(row: &Row<'_>, pac: &PacmanIndex) -> ui::SearchRow {
    let name = PkgName::new(row.picked().into_inner());
    let available = Some(row.version());
    let installed_as = row.installed_name(pac);
    // Surface the installed version (→ an `old → new` diff) only when it's
    // actually behind the available one; a same-version row just shows the
    // version.
    let old_ver = installed_as.and_then(|n| {
        pac.installed_version(n)
            .filter(|iv| {
                available
                    .as_ref()
                    .is_some_and(|av| iv.is_outdated(av.as_ver()))
            })
            .map(Version::from)
    });
    ui::SearchRow {
        repo: RepoName::from(row.repo_name()),
        name,
        install: ui::InstallState::from_installed(installed_as.is_some()),
        old_ver,
        new_ver: available,
        desc: row.desc(),
    }
}

/// Rank + sort merged repo/AUR search `rows` in place, best match first.
///
/// The order the shell list and the non-interactive listing both use:
///   1. **match tier** — a package-name *prefix* match beats a name *substring*
///      match beats a *description-only* match. (`regexes` is already applied as
///      the AND filter that produced `rows`, so every row matches *somewhere*;
///      the tier records *where*.)
///   2. **shorter name wins** within a tier — `claude` before `claude-desktop`.
///   3. repo rows sit ahead of AUR rows of otherwise-equal rank (pacman owns the
///      name), then AUR ties break **freshest-commit-first**, then name, for a
///      stable total order.
///
/// `pub(crate)` so [`crate::cli::shell`] ranks its combined list identically.
pub(crate) fn rank_rows(rows: &mut [Row<'_>], regexes: &[regex::Regex]) {
    rows.sort_by_cached_key(|r| rank_key(r, regexes));
}

/// The total-order sort key for one row — see [`rank_rows`] for the field
/// meanings. Field declaration order *is* the comparison order (derived `Ord`).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct RankKey {
    tier: MatchTier,
    name_len: usize,
    source: SourceRank,
    /// Breaks AUR ties freshest-commit-first; repo rows all tie here (they've
    /// already been separated by `source`).
    freshness: Freshness,
    /// Final lexical tie-break — the row's install identity (`PkgTarget`).
    name: PkgTarget,
}

/// A row's freshness for ranking: its AUR branch-tip commit time, ordered so
/// **fresher sorts first** — a later commit is the better tie-break. Wrapping
/// [`IndexEntry::commit_time`] keeps that "fresher wins" polarity in one
/// place (an `impl Ord`) instead of scattering a bare `Reverse<_>` through
/// the sort key.
#[derive(PartialEq, Eq)]
struct Freshness(UnixTime);

impl Freshness {
    /// Rows with no commit of their own (repo packages) — older than any real
    /// AUR commit, so they never win a freshness tie-break.
    const STALE: Self = Self(UnixTime::MIN);
}

impl Ord for Freshness {
    fn cmp(&self, other: &Self) -> Ordering {
        // Larger commit time = fresher = "less", so it lands first in the
        // best-first `RankKey` order.
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for Freshness {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Where the query matched a package's name, best to worst. Only the name
/// decides the tier; a hit that reached the row purely through its description
/// (or `provides`) lands in [`MatchTier::Desc`].
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchTier {
    /// Some name starts with the query.
    Prefix,
    /// Some name contains the whole query, but none as a prefix.
    Substring,
    /// No single name carries the whole query — it matched the description.
    Desc,
}

/// Repo rows sort ahead of AUR rows when everything else ties.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SourceRank {
    Repo,
    Aur,
}

fn rank_key(row: &Row<'_>, regexes: &[regex::Regex]) -> RankKey {
    // `picked()` is the row's install identity (repo pkgname / AUR pkgbase) —
    // the same name the label shows, reused here for the length + lexical keys.
    let name = row.picked();
    let (source, freshness) = match row {
        Row::Repo(_) => (SourceRank::Repo, Freshness::STALE),
        Row::Aur(e) => (SourceRank::Aur, Freshness(e.commit_time)),
    };
    RankKey {
        tier: match_tier(row, regexes),
        name_len: name.len(),
        source,
        freshness,
        name,
    }
}

/// The best (lowest) tier any of a row's names achieves against the whole query.
///
/// A row's names are its display name plus — for AUR split packages — each
/// member pkgname, so a query hitting only a member still counts as a name
/// match, not a description one. Each name is tiered through its typed
/// `regex_anchor` (on `PkgName` / `PkgBase`); `name_tier` combines the per-term
/// anchors into a [`MatchTier`].
fn match_tier(row: &Row<'_>, regexes: &[regex::Regex]) -> MatchTier {
    match row {
        Row::Repo(r) => name_tier(|re| r.name.regex_anchor(re), regexes),
        Row::Aur(e) => e
            .pkgnames
            .iter()
            .map(|p| name_tier(|re| p.name.regex_anchor(re), regexes))
            .fold(
                name_tier(|re| e.pkgbase.regex_anchor(re), regexes),
                MatchTier::min,
            ),
    }
}

/// Tier one name against the whole query, given `anchor` — where each term
/// matches that name. The query is an AND, so the name has to satisfy *every*
/// term (`anchor` returning `Some`) to count as a name match at all: it's
/// `Prefix` when some term anchors at the name's start, `Substring` when all
/// terms match but none anchors, else `Desc` (the row was pulled in by its
/// description). The typed [`NameMatch`] keeps an anchored query like `^foo$`
/// classified as the exact-name match it is.
fn name_tier(
    anchor: impl Fn(&regex::Regex) -> Option<NameMatch>,
    regexes: &[regex::Regex],
) -> MatchTier {
    let mut any_prefix = false;
    for r in regexes {
        match anchor(r) {
            Some(NameMatch::Prefix) => any_prefix = true,
            Some(NameMatch::Inside) => {}
            None => return MatchTier::Desc,
        }
    }
    if any_prefix {
        MatchTier::Prefix
    } else {
        MatchTier::Substring
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

    /// `[installed]` for an AUR row means "some member of this pkgbase is
    /// installed": for a split package the pkgbase name itself is not in the
    /// localdb, so the check must walk the pkgnames. Repo rows answer from
    /// the state carried on the hit.
    #[test]
    fn installed_name_finds_split_members() {
        let mut e = mk("linux-tkg", None, None);
        e.pkgnames = vec![
            Pkgname {
                name: "linux-tkg-bmq".into(),
                provides: Vec::new(),
                pkgdesc: None,
            },
            Pkgname {
                name: "linux-tkg-pds".into(),
                provides: Vec::new(),
                pkgdesc: None,
            },
        ];
        let pac = PacmanIndex {
            installed: [(PkgName::new("linux-tkg-pds"), Version::from("1-1"))].into(),
            ..Default::default()
        };
        // The pkgbase name isn't installed — only the member is.
        assert!(!pac.is_installed(&PkgName::new("linux-tkg")));
        assert_eq!(
            Row::Aur(&e).installed_name(&pac),
            Some(&PkgName::new("linux-tkg-pds"))
        );
        assert_eq!(
            Row::Repo(repo("firefox", None, true)).installed_name(&pac),
            Some(&PkgName::new("firefox"))
        );
        assert_eq!(
            Row::Repo(repo("firefox", None, false)).installed_name(&pac),
            None
        );
    }

    /// Compile domain search terms into the regexes ranking consumes.
    fn compiled(terms: &[SearchTerm]) -> Vec<regex::Regex> {
        terms.iter().map(|t| t.compile().unwrap()).collect()
    }

    /// Rank `rows` against `terms` and return the install identities in order.
    fn ranked(mut rows: Vec<Row<'_>>, terms: &[SearchTerm]) -> Vec<PkgTarget> {
        rank_rows(&mut rows, &compiled(terms));
        rows.iter().map(Row::picked).collect()
    }

    /// The primary key: a name-prefix hit outranks a name-substring hit, which
    /// outranks a description-only hit.
    #[test]
    fn rank_orders_prefix_then_substring_then_desc() {
        let substr = mk("py-claude", None, None); // "claude" at index 3
        let prefix = mk("claude", None, None);
        let desc = mk("toolkit", Some("wraps claude"), None); // name lacks the term
        let rows = vec![Row::Aur(&substr), Row::Aur(&desc), Row::Aur(&prefix)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("claude")]),
            [
                PkgTarget::from("claude"),
                PkgTarget::from("py-claude"),
                PkgTarget::from("toolkit"),
            ]
        );
    }

    /// Within a tier, the shorter name wins.
    #[test]
    fn rank_prefers_shorter_name_within_tier() {
        let long = mk("claude-desktop", None, None);
        let short = mk("claude", None, None);
        let rows = vec![Row::Aur(&long), Row::Aur(&short)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("claude")]),
            [PkgTarget::from("claude"), PkgTarget::from("claude-desktop")]
        );
    }

    /// Equal tier + equal name length: a repo row sorts ahead of an AUR one.
    #[test]
    fn rank_puts_repo_ahead_of_aur_on_equal_match() {
        let aur = mk("claude", None, None);
        let mut rows = vec![Row::Aur(&aur), Row::Repo(repo("claude", None, false))];
        rank_rows(&mut rows, &compiled(&[SearchTerm::new("claude")]));
        assert!(matches!(rows[0], Row::Repo(_)), "repo should lead the tie");
        assert!(matches!(rows[1], Row::Aur(_)));
    }

    /// `Freshness` is the domain key behind the AUR tie-break: a newer commit
    /// sorts *before* an older one, and repo rows' `STALE` sorts last.
    #[test]
    fn freshness_orders_newer_before_older() {
        assert!(Freshness(UnixTime::new(900)) < Freshness(UnixTime::new(100)));
        assert!(Freshness(UnixTime::new(100)) < Freshness::STALE);
    }

    /// End to end, that tie-break beats the lexical fallback (`aaa-` would
    /// otherwise precede `zzz-`): the fresher pkgbase leads.
    #[test]
    fn rank_breaks_aur_ties_by_freshest_commit() {
        let mut old = mk("aaa-claude", None, None);
        old.commit_time = UnixTime::new(100);
        let mut fresh = mk("zzz-claude", None, None);
        fresh.commit_time = UnixTime::new(900);
        let rows = vec![Row::Aur(&old), Row::Aur(&fresh)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("claude")]),
            [PkgTarget::from("zzz-claude"), PkgTarget::from("aaa-claude")]
        );
    }

    /// An anchored regex (`^name$`) still classifies as an exact name-prefix
    /// match — the tier is computed from the compiled regex, not raw text.
    #[test]
    fn rank_treats_anchored_regex_as_name_prefix() {
        let hit = mk("test-trivial", None, None);
        let miss = mk("unrelated", None, None);
        let rx = compiled(&[SearchTerm::new("^test-trivial$")]);
        assert_eq!(match_tier(&Row::Aur(&hit), &rx), MatchTier::Prefix);
        assert_eq!(match_tier(&Row::Aur(&miss), &rx), MatchTier::Desc);
    }

    /// Multi-term AND: a name-tier match needs *every* term in the name. Here
    /// `python-claude` carries both (→ prefix), while `claude-cli` has "python"
    /// only in its description (→ desc), so it ranks lower.
    #[test]
    fn rank_multi_term_requires_all_terms_in_name() {
        let both = mk("python-claude", None, None);
        let one = mk("claude-cli", Some("a python helper"), None);
        let rows = vec![Row::Aur(&one), Row::Aur(&both)];
        assert_eq!(
            ranked(
                rows,
                &[SearchTerm::new("python"), SearchTerm::new("claude")]
            ),
            [
                PkgTarget::from("python-claude"),
                PkgTarget::from("claude-cli")
            ]
        );
    }

    /// Render one row through the `-Ss` writer.
    fn rendered(row: &Row<'_>, install: ui::InstallState) -> String {
        let mut buf: Vec<u8> = Vec::new();
        write_search_result(&mut buf, row, install).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// pacman -Ss prints `repo/name version` then the indented description —
    /// an AUR row slots into that exact layout under its `aur/` bucket.
    #[test]
    fn search_result_format_matches_pacman_ss() {
        let e = mk("foo", Some("does foo"), None);
        let out = rendered(&Row::Aur(&e), ui::InstallState::NotInstalled);
        assert_eq!(out, "aur/foo 1.2.3-4\n    does foo\n");
    }

    /// A repo row renders under its own sync-DB bucket, same layout.
    #[test]
    fn search_result_renders_repo_rows_under_their_repo() {
        let row = Row::Repo(repo("firefox", Some("a browser"), false));
        let out = rendered(&row, ui::InstallState::NotInstalled);
        assert_eq!(out, "extra/firefox 2.0-1\n    a browser\n");
    }

    /// Installed rows carry pacman's ` [installed]` marker on the headline.
    #[test]
    fn search_result_marks_installed_rows() {
        let row = Row::Repo(repo("firefox", None, true));
        let out = rendered(&row, ui::InstallState::Installed);
        assert_eq!(out, "extra/firefox 2.0-1 [installed]\n");
    }

    /// Single-line output, no blank "    " for entries without a description.
    #[test]
    fn search_result_omits_description_block_when_none() {
        let e = mk("bar", None, None);
        let out = rendered(&Row::Aur(&e), ui::InstallState::NotInstalled);
        assert_eq!(out, "aur/bar 1.2.3-4\n");
    }

    /// The AUR version comes through `IndexEntry::version`, epoch included.
    #[test]
    fn search_result_includes_epoch_when_present() {
        let e = mk("baz", None, Some("2"));
        let out = rendered(&Row::Aur(&e), ui::InstallState::NotInstalled);
        assert_eq!(out, "aur/baz 2:1.2.3-4\n");
    }

    /// A split package's member pkgname counts as a name match, not a
    /// description one — so a query hitting only a member still ranks by name.
    #[test]
    fn rank_member_pkgname_counts_as_name_match() {
        let mut e = mk("widgets", None, None);
        e.pkgnames.push(Pkgname {
            name: PkgName::new("libclaude"),
            provides: Vec::new(),
            pkgdesc: None,
        });
        let rx = compiled(&[SearchTerm::new("claude")]);
        assert_eq!(match_tier(&Row::Aur(&e), &rx), MatchTier::Substring);
    }
}
