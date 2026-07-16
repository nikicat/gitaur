//! Package search across the sync repos + the AUR — one merged, ranked list.
//!
//! Two non-interactive surfaces live here, both wired up from
//! [`crate::cli::dispatch`]: [`cmd_search`] is `-Ss` (pacman's
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
use crate::names::{GroupName, NameMatch, PkgName, PkgTarget, RepoName, SearchTerm, VirtualName};
use crate::pacman::alpm_db::{self, PacmanIndex, RepoHit};
use crate::pacman::invoke::REPO_AUR;
use crate::ui;
use crate::units::UnixTime;
use crate::version::Version;

use std::cmp::Ordering;
use tracing::{debug, info, instrument};

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
/// "repos on top", [`rank_rows`] interleaves both sources by match quality),
/// ordered best-**last** so the strongest hit lands nearest the prompt — the
/// same bottom-up convention as the shell's numbered list.
fn merged_rows<'a>(
    repo_hits: Vec<RepoHit>,
    aur_hits: Vec<&'a IndexEntry>,
    regexes: &[regex::Regex],
) -> Vec<RankedRow<'a>> {
    let rows: Vec<Row<'a>> = repo_hits
        .into_iter()
        .map(Row::Repo)
        .chain(aur_hits.into_iter().map(Row::Aur))
        .collect();
    let mut ranked = rank_rows(rows, regexes);
    ranked.reverse();
    info!(rows = ranked.len(), "search results");
    ranked
}

/// `-Ss <regex>...` — search the sync repos and the AUR in one ranked list.
///
/// Printed in pacman's `repo/name version` format, colored per pacman's own
/// `-Ss` palette when color is on (see [`ui::search_result`]).
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
    let paint = ui::Paint::detect();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for r in &rows {
        write_search_result(&mut out, &r.row, &pac, paint)?;
    }
    Ok(0)
}

/// Write one search hit in pacman's `-Ss` format to `out` — layout and
/// palette in [`ui::search_result`]; [`search_row`] resolves the row's
/// installed state against `pac`.
///
/// Stdout (not stderr) so `aurox -Ss foo | head` works — the equivalent
/// `pacman -Ss` also writes results to stdout. A writer so the exact byte
/// layout is testable without spawning a process.
fn write_search_result<W: std::io::Write>(
    out: &mut W,
    row: &Row<'_>,
    pac: &PacmanIndex,
    paint: ui::Paint,
) -> std::io::Result<()> {
    for line in ui::search_result(&search_row(row, pac), paint).lines() {
        writeln!(out, "{line}")?;
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
/// one relevance-ranked list ([`merged_rows`]) and printed best-last, so the
/// strongest hit ends nearest the prompt. Nothing is installed:
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
    let search_rows: Vec<ui::SearchRow> = rows.iter().map(|r| r.search_row(&pac)).collect();
    let table = ui::search_table(&search_rows, &pac, &ui::PreviewMetrics::empty());
    for line in table.lines() {
        println!("{line}");
    }
    Ok(0)
}

/// Resolve one ranked [`Row`] into a [`ui::SearchRow`]: its display name,
/// available version, description, and — against `pac` — the installed state
/// carrying the localdb version, the same lookup `pacman -Ss` bases its
/// `[installed: X]` marker on. The table's `old → new` diff derives from that
/// state in the renderer ([`ui::SearchRow::upgrade_from`]).
///
/// The match-site annotation is *not* filled here (`-Ss` renders through this
/// bare form, so the pacman-parity surface can't grow a note by accident);
/// the aligned table goes through [`RankedRow::search_row`] instead.
pub(crate) fn search_row(row: &Row<'_>, pac: &PacmanIndex) -> ui::SearchRow {
    let install = row
        .installed_name(pac)
        .and_then(|n| pac.installed.get(n))
        .map_or(ui::InstallState::NotInstalled, |iv| {
            ui::InstallState::Installed(iv.clone())
        });
    ui::SearchRow {
        repo: RepoName::from(row.repo_name()),
        name: PkgName::new(row.picked().into_inner()),
        install,
        new_ver: Some(row.version()),
        desc: row.desc(),
        note: None,
    }
}

/// A search row decorated with its one-shot [`MatchClass`] — what ranking
/// produces and every renderer consumes. The classification is computed once
/// and feeds both the sort key and the match-site annotation, so the two
/// can't drift apart.
pub(crate) struct RankedRow<'a> {
    pub(crate) row: Row<'a>,
    class: MatchClass,
}

impl RankedRow<'_> {
    /// [`search_row`] plus this row's match-site annotation — the aligned
    /// table's input (the bare-term pipe listing and the shell list).
    pub(crate) fn search_row(&self, pac: &PacmanIndex) -> ui::SearchRow {
        ui::SearchRow {
            note: self.class.note.clone(),
            ..search_row(&self.row, pac)
        }
    }
}

/// Classify + rank merged repo/AUR search `rows`, best match first.
///
/// The order the shell list and the non-interactive listing both use:
///   1. **match tier** — the [`MatchTier`] ladder, from name-prefix down to
///      provides-substring. (`regexes` is already applied as the AND filter
///      that produced `rows`, so every row matches *somewhere*; the tier
///      records *where* — see [`MatchClass::of`].)
///   2. **shorter name wins** within a tier — `claude` before
///      `claude-desktop`; the length is the name that *earned* the tier, so a
///      split pkgbase pulled in by a long member ranks by that member, not by
///      its short pkgbase name.
///   3. repo rows sit ahead of AUR rows of otherwise-equal rank (pacman owns
///      the name), then AUR ties break **freshest-commit-first**, then name,
///      for a stable total order.
///
/// `pub(crate)` so [`crate::cli::shell`] ranks its combined list identically.
pub(crate) fn rank_rows<'a>(rows: Vec<Row<'a>>, regexes: &[regex::Regex]) -> Vec<RankedRow<'a>> {
    let mut ranked: Vec<RankedRow<'a>> = rows
        .into_iter()
        .map(|row| {
            let class = MatchClass::of(&row, regexes);
            RankedRow { row, class }
        })
        .collect();
    ranked.sort_by_cached_key(RankKey::of);
    let mut by_tier = [0usize; 5];
    for r in &ranked {
        by_tier[r.class.tier as usize] += 1;
    }
    debug!(
        name_prefix = by_tier[MatchTier::NamePrefix as usize],
        name_substring = by_tier[MatchTier::NameSubstring as usize],
        provides_exact = by_tier[MatchTier::ProvidesExact as usize],
        desc = by_tier[MatchTier::Desc as usize],
        provides_substring = by_tier[MatchTier::ProvidesSubstring as usize],
        "ranked search rows"
    );
    ranked
}

/// The total-order sort key for one row — see [`rank_rows`] for the field
/// meanings. Field declaration order *is* the comparison order (derived `Ord`).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct RankKey {
    tier: MatchTier,
    /// Length of the name that earned the tier (see [`MatchClass`]).
    name_len: usize,
    source: SourceRank,
    /// Breaks AUR ties freshest-commit-first; repo rows all tie here (they've
    /// already been separated by `source`).
    freshness: Freshness,
    /// Final lexical tie-break — the row's install identity (`PkgTarget`).
    name: PkgTarget,
}

impl RankKey {
    fn of(r: &RankedRow<'_>) -> Self {
        let (source, freshness) = match &r.row {
            Row::Repo(_) => (SourceRank::Repo, Freshness::STALE),
            Row::Aur(e) => (SourceRank::Aur, Freshness(e.commit_time)),
        };
        Self {
            tier: r.class.tier,
            name_len: r.class.name_len,
            source,
            freshness,
            name: r.row.picked(),
        }
    }
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

/// Where the query matched a row, best to worst — variant order *is* the
/// rank order (derived `Ord`).
///
/// The name tiers require the term(s) in a package name. `ProvidesExact` is
/// a whole-name hit on a `provides=` entry — the user typed a virtual name
/// (`wireguard-module`), so its providers outrank description matches.
/// `Desc` covers descriptions, repo groups, and the no-site fallback. And
/// `ProvidesSubstring` — a term merely *inside* a provides name, like
/// `virtualbox` in every kernel's `VIRTUALBOX-GUEST-MODULES` — sinks below
/// everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchTier {
    /// Some name starts with (or equals) the query.
    NamePrefix,
    /// Some name contains the whole query, but none as a prefix.
    NameSubstring,
    /// A bare `provides=` name is matched over its entire span.
    ProvidesExact,
    /// A description or a repo group carries the query — or nothing
    /// classifiable did (the fallback; see [`MatchClass::of`]).
    Desc,
    /// The query is merely a substring of a `provides=` name.
    ProvidesSubstring,
}

/// Repo rows sort ahead of AUR rows when everything else ties.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SourceRank {
    Repo,
    Aur,
}

/// One row's classification against the whole AND query — computed ONCE by
/// [`MatchClass::of`], read by both [`RankKey`] and the rendered
/// [`ui::MatchNote`].
struct MatchClass {
    tier: MatchTier,
    /// Byte length of the name that *earned* the tier (the best-matching
    /// member / pkgbase / repo pkgname); the display name's length for the
    /// non-name tiers.
    name_len: usize,
    /// Annotation for the bottleneck site; `None` when that site is visible
    /// on the row (display name / display desc) or unknown.
    note: Option<ui::MatchNote>,
}

impl MatchClass {
    /// Classify `row` against the whole AND query: each term takes its best
    /// site ([`Best::of`]); the row's tier and annotation come from the
    /// **worst** term — the bottleneck that kept it out of a higher tier
    /// (ties keep the first term, deterministic in user-typed order).
    ///
    /// Two deliberate differences from the pre-ladder ranking:
    /// * a name tier no longer requires all terms inside ONE name — term A
    ///   hitting member X and term B member Y still ranks as a name match;
    /// * a prefix hit for one term plus a substring hit for another ranks
    ///   `NameSubstring` (worst-of), where the old ranking reported prefix.
    fn of(row: &Row<'_>, regexes: &[regex::Regex]) -> Self {
        let mut worst: Option<Best<'_>> = None;
        for r in regexes {
            let b = Best::of(row, r);
            if worst.as_ref().is_none_or(|w| b.tier > w.tier) {
                worst = Some(b);
            }
        }
        let worst = worst.unwrap_or(Best {
            tier: MatchTier::Desc,
            site: Site::Unknown,
        });
        let display_len = row.picked().len();
        let (name_len, note) = match worst.site {
            Site::DisplayName | Site::DisplayDesc | Site::Unknown => (display_len, None),
            Site::MemberName(n) => (n.len(), Some(ui::MatchNote::Via(n.clone()))),
            Site::MemberDesc(n) => (display_len, Some(ui::MatchNote::Via(n.clone()))),
            Site::Provides(s) => (
                display_len,
                Some(ui::MatchNote::Provides(VirtualName::new(s))),
            ),
            Site::Group(g) => (display_len, Some(ui::MatchNote::Group(g.clone()))),
        };
        Self {
            tier: worst.tier,
            name_len,
            note,
        }
    }
}

/// Where one term matched a row — the classification's internal vocabulary.
/// Borrows from the row; only the final [`MatchClass`] clones.
enum Site<'e> {
    /// The name the row displays (repo pkgname / AUR pkgbase or its canonical
    /// member) — the match is visible, no annotation.
    DisplayName,
    /// A split member's pkgname, hidden behind the pkgbase row.
    MemberName(&'e PkgName),
    /// The description the row displays — visible, no annotation.
    DisplayDesc,
    /// A split member's own description that `display_desc` did not pick —
    /// hidden, so the member is named in the annotation.
    MemberDesc(&'e PkgName),
    /// A bare `provides=` name.
    Provides(&'e str),
    /// A repo package's group.
    Group(&'e GroupName),
    /// Nothing classifiable matched: a repo hit libalpm matched with POSIX
    /// ERE / plain-substring semantics our regex doesn't reproduce, or an
    /// AUR hit where the term matched only a provides *version suffix*
    /// (the index filter matches full dep specs).
    Unknown,
}

/// One term's best (lowest-tier) match site on a row.
struct Best<'e> {
    tier: MatchTier,
    site: Site<'e>,
}

impl<'e> Best<'e> {
    /// Probe every site of `row` for `r`, best tier wins; **equal tiers keep
    /// the earlier, more visible site**, so an invisible site only wins when
    /// strictly better — that is what suppresses the annotation whenever the
    /// displayed name/desc explains the row equally well.
    fn of(row: &'e Row<'_>, r: &regex::Regex) -> Self {
        let mut best: Option<Best<'e>> = None;
        let mut consider = |tier: MatchTier, site: Site<'e>| {
            if best.as_ref().is_none_or(|b| tier < b.tier) {
                best = Some(Best { tier, site });
            }
        };
        match row {
            Row::Aur(e) => {
                if let Some(t) = name_tier(e.pkgbase.regex_anchor(r)) {
                    consider(t, Site::DisplayName);
                }
                for p in &e.pkgnames {
                    if let Some(t) = name_tier(p.name.regex_anchor(r)) {
                        let site = if e.pkgbase.matches_pkgname(&p.name) {
                            Site::DisplayName
                        } else {
                            Site::MemberName(&p.name)
                        };
                        consider(t, site);
                    }
                }
                let shown = e.display_desc();
                if let Some(d) = shown
                    && r.is_match(d)
                {
                    consider(MatchTier::Desc, Site::DisplayDesc);
                }
                for p in &e.pkgnames {
                    if let Some(d) = p.pkgdesc.as_deref()
                        && !d.is_empty()
                        && shown != Some(d)
                        && r.is_match(d)
                    {
                        consider(MatchTier::Desc, Site::MemberDesc(&p.name));
                    }
                }
                for prov in e.all_provides() {
                    if let Some(t) = provides_tier(prov.bare_anchor(r)) {
                        consider(t, Site::Provides(prov.bare()));
                    }
                }
            }
            Row::Repo(h) => {
                if let Some(t) = name_tier(h.name.regex_anchor(r)) {
                    consider(t, Site::DisplayName);
                }
                if let Some(d) = h.desc.as_deref()
                    && r.is_match(d)
                {
                    consider(MatchTier::Desc, Site::DisplayDesc);
                }
                for v in &h.provides {
                    if let Some(t) = provides_tier(v.regex_anchor(r)) {
                        consider(t, Site::Provides(v.as_str()));
                    }
                }
                for g in &h.groups {
                    if g.matches_regex(r) {
                        consider(MatchTier::Desc, Site::Group(g));
                    }
                }
            }
        }
        best.unwrap_or(Best {
            tier: MatchTier::Desc,
            site: Site::Unknown,
        })
    }
}

/// Lift a name's [`NameMatch`] anchor into the name tiers. The typed anchor
/// keeps a query like `^foo$` classified as the exact-name match it is.
const fn name_tier(anchor: Option<NameMatch>) -> Option<MatchTier> {
    match anchor {
        Some(NameMatch::Exact | NameMatch::Prefix) => Some(MatchTier::NamePrefix),
        Some(NameMatch::Inside) => Some(MatchTier::NameSubstring),
        None => None,
    }
}

/// Lift a provides name's anchor into the provides tiers: only a whole-span
/// hit counts as the user naming the virtual; anything else is the noise
/// tier below descriptions.
const fn provides_tier(anchor: Option<NameMatch>) -> Option<MatchTier> {
    match anchor {
        Some(NameMatch::Exact) => Some(MatchTier::ProvidesExact),
        Some(NameMatch::Prefix | NameMatch::Inside) => Some(MatchTier::ProvidesSubstring),
        None => None,
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
            provides: Vec::new(),
            groups: Vec::new(),
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
    fn ranked(rows: Vec<Row<'_>>, terms: &[SearchTerm]) -> Vec<PkgTarget> {
        rank_rows(rows, &compiled(terms))
            .iter()
            .map(|r| r.row.picked())
            .collect()
    }

    /// Classify one row against `terms` — the tier + annotation both rank and
    /// render read.
    fn class(row: &Row<'_>, terms: &[SearchTerm]) -> MatchClass {
        MatchClass::of(row, &compiled(terms))
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
        let rows = vec![Row::Aur(&aur), Row::Repo(repo("claude", None, false))];
        let ranked = rank_rows(rows, &compiled(&[SearchTerm::new("claude")]));
        assert!(
            matches!(ranked[0].row, Row::Repo(_)),
            "repo should lead the tie"
        );
        assert!(matches!(ranked[1].row, Row::Aur(_)));
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
        let terms = [SearchTerm::new("^test-trivial$")];
        assert_eq!(class(&Row::Aur(&hit), &terms).tier, MatchTier::NamePrefix);
        assert_eq!(class(&Row::Aur(&miss), &terms).tier, MatchTier::Desc);
    }

    /// Multi-term AND: the row ranks by its *bottleneck* term. `python-claude`
    /// carries both terms in its name (worst site: name), while `claude-cli`
    /// has "python" only in its description (worst site: desc), so it ranks
    /// lower.
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

    /// Render one row through the `-Ss` writer, plain paint — the byte layout
    /// pinned here; the ANSI form is pinned in `ui::search_table`'s tests.
    /// Installed state (and its version) resolves against `pac`, the same
    /// localdb lookup pacman bases its marker on.
    fn rendered(row: &Row<'_>, pac: &PacmanIndex) -> String {
        let mut buf: Vec<u8> = Vec::new();
        write_search_result(&mut buf, row, pac, ui::Paint::Plain).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// pacman -Ss prints `repo/name version` then the indented description —
    /// an AUR row slots into that exact layout under its `aur/` bucket.
    #[test]
    fn search_result_format_matches_pacman_ss() {
        let e = mk("foo", Some("does foo"), None);
        let out = rendered(&Row::Aur(&e), &PacmanIndex::default());
        assert_eq!(out, "aur/foo 1.2.3-4\n    does foo\n");
    }

    /// A repo row renders under its own sync-DB bucket, same layout.
    #[test]
    fn search_result_renders_repo_rows_under_their_repo() {
        let row = Row::Repo(repo("firefox", Some("a browser"), false));
        let out = rendered(&row, &PacmanIndex::default());
        assert_eq!(out, "extra/firefox 2.0-1\n    a browser\n");
    }

    /// Installed rows carry pacman's ` [installed]` marker on the headline
    /// when the localdb version matches the listed one.
    #[test]
    fn search_result_marks_installed_rows() {
        let mut pac = PacmanIndex::default();
        pac.installed
            .insert("firefox".into(), Version::from("2.0-1"));
        let row = Row::Repo(repo("firefox", None, true));
        let out = rendered(&row, &pac);
        assert_eq!(out, "extra/firefox 2.0-1 [installed]\n");
    }

    /// A localdb version differing from the listed one rides in the marker,
    /// pacman-style: ` [installed: X]`.
    #[test]
    fn search_result_marks_installed_version_drift() {
        let mut pac = PacmanIndex::default();
        pac.installed
            .insert("firefox".into(), Version::from("1.9-1"));
        let row = Row::Repo(repo("firefox", None, true));
        let out = rendered(&row, &pac);
        assert_eq!(out, "extra/firefox 2.0-1 [installed: 1.9-1]\n");
    }

    /// Single-line output, no blank "    " for entries without a description.
    #[test]
    fn search_result_omits_description_block_when_none() {
        let e = mk("bar", None, None);
        let out = rendered(&Row::Aur(&e), &PacmanIndex::default());
        assert_eq!(out, "aur/bar 1.2.3-4\n");
    }

    /// The AUR version comes through `IndexEntry::version`, epoch included.
    #[test]
    fn search_result_includes_epoch_when_present() {
        let e = mk("baz", None, Some("2"));
        let out = rendered(&Row::Aur(&e), &PacmanIndex::default());
        assert_eq!(out, "aur/baz 2:1.2.3-4\n");
    }

    /// A split package's member pkgname counts as a name match, not a
    /// description one — so a query hitting only a member still ranks by name,
    /// and the hidden member is named in the annotation.
    #[test]
    fn rank_member_pkgname_counts_as_name_match() {
        let mut e = mk("widgets", None, None);
        e.pkgnames.push(Pkgname {
            name: PkgName::new("libclaude"),
            provides: Vec::new(),
            pkgdesc: None,
        });
        let c = class(&Row::Aur(&e), &[SearchTerm::new("claude")]);
        assert_eq!(c.tier, MatchTier::NameSubstring);
        assert_eq!(c.name_len, "libclaude".len(), "ranks by the earned member");
        assert_eq!(c.note, Some(ui::MatchNote::Via(PkgName::new("libclaude"))));
    }

    /// The full five-rung ladder in one list: name-prefix, name-substring,
    /// exact-provides, description, provides-substring — in that order.
    #[test]
    fn rank_orders_full_tier_ladder() {
        let prefix = mk("virtualbox-bin", None, None);
        let substr = mk("mini-virtualbox", None, None);
        let mut pexact = mk("kernel-a", None, None);
        pexact.provides = vec![PkgTarget::new("virtualbox")];
        let desc = mk("qemu-thing", Some("a virtualbox alternative"), None);
        let mut psub = mk("kernel-b", None, None);
        psub.provides = vec![PkgTarget::new("VIRTUALBOX-GUEST-MODULES")];
        let rows = vec![
            Row::Aur(&psub),
            Row::Aur(&desc),
            Row::Aur(&pexact),
            Row::Aur(&substr),
            Row::Aur(&prefix),
        ];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("virtualbox")]),
            [
                PkgTarget::from("virtualbox-bin"),
                PkgTarget::from("mini-virtualbox"),
                PkgTarget::from("kernel-a"),
                PkgTarget::from("qemu-thing"),
                PkgTarget::from("kernel-b"),
            ]
        );
    }

    /// The kernel-flood case: a term merely inside a provides name sinks
    /// below a description match, no matter how the names compare.
    #[test]
    fn rank_provides_substring_sinks_below_desc() {
        let mut kernel = mk("linux-zz", None, None);
        kernel.provides = vec![PkgTarget::new("VIRTUALBOX-GUEST-MODULES")];
        let desc = mk("vbox-tools", Some("tools for virtualbox guests"), None);
        let rows = vec![Row::Aur(&kernel), Row::Aur(&desc)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("virtualbox")]),
            [PkgTarget::from("vbox-tools"), PkgTarget::from("linux-zz")]
        );
    }

    /// Typing a virtual name exactly is a legitimate lookup: the provider
    /// outranks description matches, but still trails real name matches.
    #[test]
    fn rank_provides_exact_outranks_desc_but_not_name() {
        let name = mk("virtualbox-guest-modules-lts", None, None);
        let mut provider = mk("linux-zz", None, None);
        provider.provides = vec![PkgTarget::new("VIRTUALBOX-GUEST-MODULES")];
        let desc = mk("docs", Some("about virtualbox-guest-modules"), None);
        let rows = vec![Row::Aur(&desc), Row::Aur(&provider), Row::Aur(&name)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("virtualbox-guest-modules")]),
            [
                PkgTarget::from("virtualbox-guest-modules-lts"),
                PkgTarget::from("linux-zz"),
                PkgTarget::from("docs"),
            ]
        );
    }

    /// The openrc-misc shape: a short pkgbase pulled in by a long member must
    /// rank by the member's length, not jump the queue on its own short name.
    #[test]
    fn rank_uses_earned_member_name_length() {
        let mut openrc = mk("openrc-misc", None, None);
        openrc.pkgnames = vec![Pkgname {
            name: PkgName::new("virtualbox-guest-utils-openrc"),
            provides: Vec::new(),
            pkgdesc: None,
        }];
        let bin = mk("virtualbox-bin", None, None);
        let rows = vec![Row::Aur(&openrc), Row::Aur(&bin)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("virtualbox")]),
            [
                PkgTarget::from("virtualbox-bin"),
                PkgTarget::from("openrc-misc"),
            ]
        );
    }

    /// Worst-of across terms: one term a name prefix, the other only a name
    /// substring → the row ranks `NameSubstring` (the pre-ladder ranking
    /// reported prefix here). Pins the deliberate semantics shift.
    #[test]
    fn classify_multi_term_worst_of_demotes_prefix_to_substring() {
        let e = mk("python-claude", None, None);
        let both = [SearchTerm::new("python"), SearchTerm::new("claude")];
        assert_eq!(class(&Row::Aur(&e), &both).tier, MatchTier::NameSubstring);
        let one = [SearchTerm::new("python")];
        assert_eq!(class(&Row::Aur(&e), &one).tier, MatchTier::NamePrefix);
    }

    /// Matches on what the row already displays — its name, its description —
    /// carry no annotation.
    #[test]
    fn classify_display_sites_carry_no_note() {
        let name = mk("claude", None, None);
        let desc = mk("toolkit", Some("wraps claude"), None);
        let terms = [SearchTerm::new("claude")];
        let c = class(&Row::Aur(&name), &terms);
        assert_eq!((c.tier, c.note), (MatchTier::NamePrefix, None));
        let c = class(&Row::Aur(&desc), &terms);
        assert_eq!((c.tier, c.note), (MatchTier::Desc, None));
    }

    /// A hidden member description (pkgbase-level desc displayed instead)
    /// names the member in the annotation.
    #[test]
    fn classify_notes_hidden_member_desc() {
        let mut e = mk("widgets", Some("a widget kit"), None);
        e.pkgnames[0].pkgdesc = Some("claude bindings".to_owned());
        let c = class(&Row::Aur(&e), &[SearchTerm::new("claude")]);
        assert_eq!(c.tier, MatchTier::Desc);
        assert_eq!(c.note, Some(ui::MatchNote::Via(PkgName::new("widgets"))));
    }

    /// A versioned provides spec (`myvirt=2.5`) classifies against its bare
    /// name: `^myvirt$` is an exact provides hit, and the annotation carries
    /// the stripped name.
    #[test]
    fn classify_provides_note_strips_constraint() {
        let mut e = mk("test-provides-virt", None, None);
        e.provides = vec![PkgTarget::new("myvirt=2.5")];
        let c = class(&Row::Aur(&e), &[SearchTerm::new("^myvirt$")]);
        assert_eq!(c.tier, MatchTier::ProvidesExact);
        assert_eq!(
            c.note,
            Some(ui::MatchNote::Provides(VirtualName::new("myvirt")))
        );
    }

    /// With multiple terms, the *bottleneck* term (the worst site) decides
    /// both the tier and the annotation: name hit + provides-only hit →
    /// provides tier, provides note.
    #[test]
    fn classify_bottleneck_term_drives_tier_and_note() {
        let mut e = mk("claude-extras", None, None);
        e.provides = vec![PkgTarget::new("VIRTUALBOX-GUEST-MODULES")];
        let c = class(
            &Row::Aur(&e),
            &[SearchTerm::new("claude"), SearchTerm::new("virtualbox")],
        );
        assert_eq!(c.tier, MatchTier::ProvidesSubstring);
        assert_eq!(
            c.note,
            Some(ui::MatchNote::Provides(VirtualName::new(
                "VIRTUALBOX-GUEST-MODULES"
            )))
        );
    }

    /// libalpm's `-Ss` also matches pacman groups; a group-only repo hit is
    /// tiered with descriptions and explained by a `[group …]` note.
    #[test]
    fn classify_repo_group_match_notes_group() {
        let mut h = repo("qemu-zz", None, false);
        h.groups = vec![GroupName::new("virt-tools")];
        let c = class(&Row::Repo(h), &[SearchTerm::new("virt-tools")]);
        assert_eq!(c.tier, MatchTier::Desc);
        assert_eq!(
            c.note,
            Some(ui::MatchNote::Group(GroupName::new("virt-tools")))
        );
    }

    /// A repo hit our regex can't re-classify (libalpm matched it with POSIX
    /// ERE / plain-substring semantics) falls back to the description tier,
    /// unannotated — never dropped, never crashing.
    #[test]
    fn classify_unmatched_repo_hit_falls_back_unannotated() {
        let c = class(
            &Row::Repo(repo("weird", None, false)),
            &[SearchTerm::new("zzz")],
        );
        assert_eq!((c.tier, c.note), (MatchTier::Desc, None));
    }
}
