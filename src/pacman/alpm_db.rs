//! Read-only alpm handle helpers + a precomputed `PacmanIndex` snapshot.
//!
//! `alpm::Alpm` is `Send` but not `Sync`, so we can't share it across rayon
//! workers. `PacmanIndex` reads everything we need from `&Alpm` once into
//! owned hash structures, making subsequent lookups pure data — Sync, cheap,
//! and parallelisable.

use super::sync;
use crate::error::{Error, Result};
use crate::index::schema::IndexEntry;
use crate::index::secondary::strip_version_constraint;
use crate::names::{PkgName, PkgTarget};
use crate::version::{Ver, Version};
use alpm::Alpm;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{debug, instrument};

/// Parse the host's `pacman.conf` (via `pacman-conf`). Shared by every
/// alpm-handle builder so the config is read one consistent way.
fn load_pacman_conf() -> Result<pacmanconf::Config> {
    pacmanconf::Config::new().map_err(|e| Error::other(format!("read pacman.conf: {e}")))
}

/// What the user currently has installed that this AUR entry will displace.
///
/// Resolved across pkgname, `replaces`, and `provides`, with provenance
/// preserved so callers can render the right label.
///
/// `pkgname` is the localdb pkgname (typed [`PkgName`]); `version` is the
/// pacman-recorded `pkgver-pkgrel` of that pkg (never the virtual version
/// from a `provides=name=X` suffix), typed [`Ver`] so vercmp comparisons
/// stay correct; `via` describes how the AUR entry matched it. Lifetimes:
/// `pkgname` borrows from the [`IndexEntry`], `version` borrows from the
/// [`PacmanIndex`].
///
/// No `Eq` — `Ver`'s `PartialEq` is vercmp, which doesn't satisfy `Eq`'s
/// reflexivity guarantee in the bytes-distinct-but-vercmp-equal corner
/// case. The struct is compared by `==` only in tests, which use
/// `PartialEq`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InstalledCounterpart<'a> {
    pub pkgname: &'a PkgName,
    pub version: &'a Ver,
    pub via: MatchedVia,
}

/// How the AUR entry referenced its installed counterpart. Priority for
/// resolution is `Pkgname` > `Replaces` > `Provides` (see
/// [`PacmanIndex::counterpart`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedVia {
    /// One of the entry's own pkgnames is installed under that exact name.
    /// Canonical and split-pkg cases.
    Pkgname,
    /// `entry.replaces` names an installed pkg — strongest rename signal
    /// because the maintainer explicitly declared this build supersedes it.
    Replaces,
    /// A `provides` entry (pkgname-scoped or pkgbase-level) names an
    /// installed pkg. Weaker heuristic, but how AUR pkgbase renames
    /// typically manifest in practice (e.g. `dotnet-core-7.0-bin` providing
    /// `dotnet-runtime-7.0`).
    Provides,
}

/// One sync-repo package matching a search query.
///
/// Owned so it outlives the `Alpm` handle (which is `!Sync` and borrows its
/// `Package`s from the open DB). `repo` is the sync-DB name (`core`, `extra`,
/// …) in pacman.conf precedence order; `installed` flags rows the user already
/// has so the picker can mark them the way `pacman -Ss` does.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoHit {
    pub repo: String,
    pub name: PkgName,
    pub version: Version,
    pub desc: Option<String>,
    pub installed: bool,
}

/// Search every sync repo for packages matching all `terms` (pacman `-Ss`
/// AND semantics over name + description), returning owned hits.
///
/// Mirrors `pacman -Ss`: the first sync DB (pacman.conf order) that carries a
/// pkgname wins, so a name shadowed in a later repo isn't listed twice. Used
/// by the `gaur <term>` picker to show repo packages alongside AUR ones the
/// way yay/paru do.
#[instrument]
pub fn search_sync(terms: &[String]) -> Result<Vec<RepoHit>> {
    let alpm = open()?;
    let installed: HashSet<String> = alpm
        .localdb()
        .pkgs()
        .iter()
        .map(|p| p.name().to_owned())
        .collect();
    let mut hits = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for db in alpm.syncdbs() {
        let matches = db
            .search(terms.iter().map(String::as_str))
            .map_err(|e| Error::other(format!("search {}: {e}", db.name())))?;
        for p in &matches {
            // First DB declaring the name wins, matching pacman's repo
            // precedence — skip a name a higher-priority repo already produced.
            if !seen.insert(p.name().to_owned()) {
                continue;
            }
            hits.push(RepoHit {
                repo: db.name().to_owned(),
                name: PkgName::new(p.name()),
                version: Version::from(p.version()),
                desc: p.desc().map(str::to_owned),
                installed: installed.contains(p.name()),
            });
        }
    }
    debug!(count = hits.len(), "repo search hits");
    Ok(hits)
}

/// Open the system alpm DB with sync repos registered from `pacman.conf`.
///
/// `Alpm::new` alone gives an empty `syncdbs()` — sync repos are pacman.conf
/// state, not alpm state. We parse the config and let `alpm-utils` register
/// every `[repo]` section.
pub fn open() -> Result<Alpm> {
    build_alpm(None, None)
}

/// Like [`open`], but reading sync repos from gitaur's rootless sync db.
///
/// When that db has been populated (see [`crate::pacman::sync::refresh_sync_db`]),
/// upgrade checks reflect the just-fetched official-repo versions without a
/// privileged `pacman -Sy`. Installed-package reads are identical either way —
/// the private dbpath's `local` is a symlink to the system one.
///
/// Falls back to [`open`] (system dbpath) until the first successful refresh,
/// and is deliberately *not* used on the install path: `pacman -S` runs against
/// the system db, so resolving installs against a fresher store could plan a
/// version pacman wouldn't yet have.
pub fn open_synced() -> Result<Alpm> {
    build_alpm(sync::synced_db_path().as_deref(), None)
}

/// Open a handle aimed at gitaur's private dbpath for a *write* (the rootless
/// `syncdbs().update()`), used by [`sync::refresh_sync_db`].
///
/// The logfile is redirected to `/dev/null`: a sync-db update may `logaction`,
/// and the system log (`/var/log/pacman.log`) is root-owned — writing it as a
/// normal user would fail. We want the download, not an audit line.
pub(crate) fn open_at_for_refresh(db: &Path) -> Result<Alpm> {
    build_alpm(Some(db), Some(Path::new("/dev/null")))
}

/// The system pacman dbpath — the effective `DBPath`, read via `pacman-conf`
/// (so a `pacman.conf` override is honored, falling back to pacman's own
/// compiled default). We don't guess a path if the lookup fails: a wrong value
/// would symlink `local` to the wrong store and silently corrupt the upgrade
/// check, so the error propagates and the refresh degrades to a warning.
pub(crate) fn system_db_path() -> Result<PathBuf> {
    Ok(PathBuf::from(load_pacman_conf()?.db_path))
}

/// Open alpm with sync repos from `pacman.conf`, optionally overriding the
/// dbpath and/or logfile. `dbpath = None` uses the system dbpath; `Some(db)`
/// points at a private store (whose `local` must resolve to a real localdb).
fn build_alpm(dbpath: Option<&Path>, logfile: Option<&Path>) -> Result<Alpm> {
    let mut conf = load_pacman_conf()?;
    if let Some(db) = dbpath {
        debug!(dbpath = %db.display(), "alpm reading sync repos from gitaur's rootless db");
        conf.db_path = db.to_string_lossy().into_owned();
    }
    if let Some(lf) = logfile {
        conf.log_file = lf.to_string_lossy().into_owned();
    }
    alpm_utils::alpm_with_conf(&conf).map_err(|e| Error::other(format!("open alpm with conf: {e}")))
}

/// Snapshot of the local + sync pacman DBs as immutable hash structures.
///
/// Built once at the top of `cmd_install` so per-target classification is
/// a sequence of `HashMap` / `HashSet` lookups — Sync (no `&Alpm` to share),
/// O(1) per query, and safe to call from rayon workers.
#[derive(Debug, Default)]
pub struct PacmanIndex {
    /// pkgname → installed version (from localdb). Keys are typed
    /// `PkgName`; values are typed `Version` so `<` / `==` against a sync
    /// version automatically uses vercmp.
    pub installed: HashMap<PkgName, Version>,
    /// virtual provide name → installed pkgnames declaring it. Used to mark
    /// a dependency as already-satisfied — if any provider is installed,
    /// `pacman -S --needed` would no-op, so the plan must drop it instead
    /// of pretending to install a virtual. Keys stay `String` because
    /// `provides=foo.so` virtual names aren't pkgnames in their own right.
    pub installed_providers: HashMap<String, Vec<PkgName>>,
    /// pkgname → version available in some sync repo. Repo precedence is
    /// pacman's: the first DB declared in `pacman.conf` wins on duplicates.
    pub sync_versions: HashMap<PkgName, Version>,
    /// virtual provide name → sync-repo pkgnames declaring it. When a
    /// dependency is a virtual name we pick a concrete provider so the plan
    /// shows the package pacman would actually install, with its version.
    pub sync_providers: HashMap<String, Vec<PkgName>>,
    /// pkgname → its `depends`, recorded on the same first-DB-wins basis as
    /// [`Self::sync_versions`] so the deps read match the package pacman
    /// would actually install. Values are [`PkgTarget`] — a dep is an
    /// unclassified, possibly version-suffixed reference (`glibc>=2.38`,
    /// `libalpm.so`), exactly the shape the resolver classifies. Lets the
    /// resolver walk repo→repo edges so a repo package's not-yet-installed
    /// deps surface in the plan instead of being pulled in silently by the
    /// final `pacman -S`.
    pub sync_depends: HashMap<PkgName, Vec<PkgTarget>>,
    /// pkgname → installed on-disk size in bytes (localdb `isize()`). The
    /// upgrade loop's change-set preview reads this to estimate an AUR build's
    /// footprint from the version currently installed — a hint for ranking
    /// "small vs huge," not a contract (the rebuilt version may differ in
    /// size). Read here so the picker never has to hold an `&Alpm`.
    pub installed_size: HashMap<PkgName, u64>,
    /// pkgname → bytes pacman would fetch to install it, from the first syncdb
    /// declaring it (`download_size()`, same first-DB-wins precedence as
    /// [`Self::sync_versions`]). Already-cached packages report 0 — correct,
    /// since pacman won't re-download them. Drives the change-set preview's
    /// exact size figure for repo rows and repo deps.
    pub sync_download_size: HashMap<PkgName, u64>,
}

impl PacmanIndex {
    /// Snapshot `&Alpm` into owned hash tables. Single pass over each DB.
    /// `PkgName` wraps each pkg name at the alpm boundary — this is the
    /// single entry point that promotes raw `&str` from libalpm into the
    /// typed identity used by the rest of the crate.
    #[instrument(skip(alpm))]
    pub fn build(alpm: &Alpm) -> Self {
        let mut installed: HashMap<PkgName, Version> = HashMap::new();
        let mut installed_providers: HashMap<String, Vec<PkgName>> = HashMap::new();
        let mut installed_size: HashMap<PkgName, u64> = HashMap::new();
        for p in alpm.localdb().pkgs() {
            let name = PkgName::new(p.name());
            // `From<&alpm::Ver> for Version` reads the bytes directly via
            // `alpm::Ver::as_str()` — not through `Display::to_string`.
            installed.insert(name.clone(), Version::from(p.version()));
            // `isize()` is libalpm's `i64` (never negative in practice);
            // `try_from` floors a corrupt-DB negative at 0 rather than wrapping.
            installed_size.insert(name.clone(), u64::try_from(p.isize()).unwrap_or(0));
            for prov in p.provides() {
                installed_providers
                    .entry(prov.name().to_owned())
                    .or_default()
                    .push(name.clone());
            }
        }
        let mut sync_versions: HashMap<PkgName, Version> = HashMap::new();
        let mut sync_providers: HashMap<String, Vec<PkgName>> = HashMap::new();
        let mut sync_depends: HashMap<PkgName, Vec<PkgTarget>> = HashMap::new();
        let mut sync_download_size: HashMap<PkgName, u64> = HashMap::new();
        for db in alpm.syncdbs() {
            for p in db.pkgs() {
                let name = PkgName::new(p.name());
                // First DB pacman.conf lists wins, matching pacman's own repo
                // precedence — record version + depends + download size
                // together on that first sighting so they describe the same
                // package. (provides stay accumulated across DBs below;
                // `resolve_concrete` only reads the first provider, so
                // duplicates are harmless.)
                if !sync_versions.contains_key(&name) {
                    sync_versions.insert(name.clone(), Version::from(p.version()));
                    // `download_size()` is the bytes pacman would fetch (0 when
                    // the archive is already cached); `try_from` floors a
                    // negative at 0, as with `isize` above.
                    sync_download_size
                        .insert(name.clone(), u64::try_from(p.download_size()).unwrap_or(0));
                    // `Dep::name()` is the bare dependency name (alpm parses
                    // off the version constraint); the resolver classifies on
                    // that name, so widen it straight into an unclassified
                    // `PkgTarget` rather than carrying the constraint.
                    sync_depends.insert(
                        name.clone(),
                        p.depends()
                            .iter()
                            .map(|d| PkgTarget::from(d.name()))
                            .collect(),
                    );
                }
                for prov in p.provides() {
                    sync_providers
                        .entry(prov.name().to_owned())
                        .or_default()
                        .push(name.clone());
                }
            }
        }
        debug!(
            installed = installed.len(),
            installed_provides = installed_providers.len(),
            sync = sync_versions.len(),
            sync_provides = sync_providers.len(),
            sync_depends = sync_depends.len(),
            "pacman index built"
        );
        Self {
            installed,
            installed_providers,
            sync_versions,
            sync_providers,
            sync_depends,
            installed_size,
            sync_download_size,
        }
    }

    /// Installed version of `name`, or `None` if not installed. `name`
    /// arrives as `&str` because lookups originate from many sources
    /// (CLI args, .SRCINFO deps, `provides` strings) — `Borrow<str>` on
    /// the typed key makes the lookup work without a temporary `PkgName`.
    /// Returns `&Ver` so the caller can compare via vercmp.
    pub fn installed_version(&self, name: &str) -> Option<&Ver> {
        self.installed.get(name).map(Version::as_ver)
    }

    /// Already installed locally?
    pub fn is_installed(&self, name: &str) -> bool {
        self.installed.contains_key(name)
    }

    /// Available in a sync repo, either by exact name or by virtual provide?
    pub fn in_sync(&self, name: &str) -> bool {
        self.sync_versions.contains_key(name) || self.sync_providers.contains_key(name)
    }

    /// Sync-repo version for `name`, or `None` when `name` is not an exact
    /// pkgname in any syncdb. Matches by-name only — virtual `provides` aren't
    /// versioned (their version, if any, lives on the providing pkg) so a
    /// provides hit deliberately returns `None`.
    pub fn sync_version(&self, name: &str) -> Option<&Ver> {
        self.sync_versions.get(name).map(Version::as_ver)
    }

    /// Sync-repo `depends` for the concrete pkgname `name`, or an empty slice
    /// when `name` isn't an exact sync pkgname. Takes `&PkgName` (not `&str`
    /// like the version lookups): a dep walk only makes sense once a name has
    /// been resolved to a concrete provider — virtual provides carry no deps
    /// of their own — so the type encodes that precondition. Drives the
    /// resolver's repo→repo dependency walk.
    pub fn sync_depends(&self, name: &PkgName) -> &[PkgTarget] {
        self.sync_depends.get(name).map_or(&[], Vec::as_slice)
    }

    /// Installed on-disk size of `name` in bytes, or `None` when it isn't in
    /// localdb. Takes `&PkgName` (not `&str`): a size estimate only makes sense
    /// for a concrete installed pkgname, the shape the change-set preview
    /// already holds. Backs the AUR-row footprint estimate.
    pub fn installed_size(&self, name: &PkgName) -> Option<u64> {
        self.installed_size.get(name).copied()
    }

    /// Bytes pacman would download for `name` (0 if already cached), or `None`
    /// when `name` isn't an exact sync pkgname. Backs the exact size figure for
    /// repo rows and repo deps in the change-set preview.
    pub fn sync_download_size(&self, name: &PkgName) -> Option<u64> {
        self.sync_download_size.get(name).copied()
    }

    /// Resolve a (possibly virtual) name to the concrete pkgname pacman would
    /// act on, paired with whether it's already installed.
    ///
    /// Order:
    ///   1. exact installed pkgname → `(name, true)`
    ///   2. an installed pkg providing the virtual → `(provider, true)`
    ///   3. exact sync pkgname → `(name, false)`
    ///   4. a sync pkg providing the virtual → `(first_provider, false)`
    ///   5. nothing pacman knows about → `None`
    ///
    /// "Installed providers win" is the load-bearing choice: `pacman -S --needed`
    /// on an already-satisfied virtual is a no-op, so the plan must drop the
    /// dep instead of staging a redundant install of a different concrete pkg.
    /// On a sync-providers tie we pick the first one we saw (DB declaration
    /// order from `pacman.conf`); pacman would prompt, we don't.
    pub fn resolve_concrete(&self, name: &str) -> Option<(&PkgName, bool)> {
        if let Some((n, _)) = self.installed.get_key_value(name) {
            return Some((n, true));
        }
        if let Some(provs) = self.installed_providers.get(name)
            && let Some(p) = provs.first()
        {
            return Some((p, true));
        }
        if let Some((n, _)) = self.sync_versions.get_key_value(name) {
            return Some((n, false));
        }
        if let Some(provs) = self.sync_providers.get(name)
            && let Some(p) = provs.first()
        {
            return Some((p, false));
        }
        None
    }

    /// Resolve the installed pkg an [`IndexEntry`] would displace, classified
    /// by how the AUR entry referenced it.
    ///
    /// Resolution order (highest priority first):
    ///   1. **Pkgname** — any `entry.pkgnames[*].name` present in localdb.
    ///      Canonical case and split pkgs (Bisq shape) land here.
    ///   2. **Replaces** — any bare name in `entry.replaces` present in
    ///      localdb. The maintainer's explicit "this build supersedes that
    ///      pkg" declaration.
    ///   3. **Provides** — any bare name in `entry.pkgnames[*].provides`
    ///      (scoped) or `entry.provides` (pkgbase-level) present in localdb.
    ///      The implicit transition path AUR pkgbase renames usually take
    ///      (e.g. `dotnet-core-7.0-bin` providing `dotnet-runtime-7.0`).
    ///
    /// Within each tier the first hit wins, in the entry's declaration
    /// order — `Vec` ordering is stable across runs, so the choice is
    /// deterministic. Names with a version constraint suffix
    /// (`provides = libfoo=1.2`) go through [`strip_version_constraint`]
    /// before lookup; the returned `version` is **always** the pacman
    /// localdb version of the matched pkgname, never the virtual version
    /// baked into the suffix.
    ///
    /// Returns `None` when nothing in the entry matches an installed pkg —
    /// the caller renders this as a fresh install.
    pub fn counterpart<'a>(&'a self, entry: &'a IndexEntry) -> Option<InstalledCounterpart<'a>> {
        self.counterpart_with_hint(entry, None)
    }

    /// Like [`Self::counterpart`] but biased by a user-supplied `hint` —
    /// the pkgname the user typed (or the picker handed us) that they think
    /// they have installed.
    ///
    /// When `hint` is present and matches an installed pkgname covered by
    /// the entry's pkgnames / replaces / provides, the lookup returns that
    /// match directly (with the appropriate provenance), short-circuiting
    /// the unhinted "first hit wins" walk. This fixes the dotnet-runtime
    /// regression: a pkgbase with multiple `provides=` virtuals
    /// (`aspnet-runtime`, `dotnet-runtime-7.0`, …) where more than one is
    /// installed would otherwise pick the first one in declaration order,
    /// not the one the user actually asked about.
    ///
    /// When `hint` is `None` or doesn't match anything in the entry, falls
    /// through to the unhinted walk — semantics preserved for callers that
    /// don't have a hint.
    ///
    /// Emits two diagnostic warnings:
    ///   * **hint divergence** — when the hint changed the picked pkgname
    ///     from what the unhinted walk would have returned. The unhinted
    ///     pick was wrong; the hint rescued it. Future bugs of the
    ///     dotnet-runtime shape will show up in the trace immediately.
    ///   * **multi-match** — when the unhinted walk has more than one
    ///     installed candidate in its provides tier. The pick is arbitrary
    ///     without a hint to disambiguate; logging the alternatives makes
    ///     "why did review label X instead of Y?" answerable.
    pub fn counterpart_with_hint<'a>(
        &'a self,
        entry: &'a IndexEntry,
        hint: Option<&PkgName>,
    ) -> Option<InstalledCounterpart<'a>> {
        let unhinted = self.counterpart_unhinted(entry);
        let result = hint
            .and_then(|h| self.counterpart_for_hint(entry, h))
            .or(unhinted);
        if let (Some(h), Some(r), Some(u)) = (hint, result, unhinted)
            && r.pkgname != u.pkgname
        {
            tracing::warn!(
                pkgbase = %entry.pkgbase,
                hint = %h,
                hinted = %r.pkgname,
                unhinted = %u.pkgname,
                "counterpart hint diverged from unhinted lookup",
            );
        }
        result
    }

    /// Single-hint probe: if `hint` is installed AND the entry references it
    /// (as pkgname, replaces, or provides), return that match. Skipped when
    /// the hint isn't installed — there's no counterpart to anchor on, so
    /// fall back to the unhinted walk.
    fn counterpart_for_hint<'a>(
        &'a self,
        entry: &'a IndexEntry,
        hint: &PkgName,
    ) -> Option<InstalledCounterpart<'a>> {
        let (stored_name, version) = self.installed.get_key_value(hint)?;
        if entry.pkgnames.iter().any(|p| p.name == *stored_name) {
            return Some(InstalledCounterpart {
                pkgname: stored_name,
                version,
                via: MatchedVia::Pkgname,
            });
        }
        if entry
            .replaces
            .iter()
            .any(|r| stored_name == strip_version_constraint(r))
        {
            return Some(InstalledCounterpart {
                pkgname: stored_name,
                version,
                via: MatchedVia::Replaces,
            });
        }
        let in_scoped_provides = entry
            .pkgnames
            .iter()
            .flat_map(|p| &p.provides)
            .any(|prov| stored_name == strip_version_constraint(prov));
        let in_pkgbase_provides = entry
            .provides
            .iter()
            .any(|prov| stored_name == strip_version_constraint(prov));
        if in_scoped_provides || in_pkgbase_provides {
            return Some(InstalledCounterpart {
                pkgname: stored_name,
                version,
                via: MatchedVia::Provides,
            });
        }
        None
    }

    /// The original unhinted walk — extracted so the hinted path can fall
    /// back to it. Pkgname / Replaces tiers short-circuit on the first
    /// match (multiple installed siblings of a split pkgbase are normal,
    /// and any of them produces the same review header). The Provides
    /// tier instead collects *all* matches before picking, so the call can
    /// emit a `multi-match` warning when more than one provider is
    /// installed — the dotnet-runtime shape the hint plumbing exists to
    /// disambiguate.
    fn counterpart_unhinted<'a>(
        &'a self,
        entry: &'a IndexEntry,
    ) -> Option<InstalledCounterpart<'a>> {
        // 1. Direct pkgname match. `installed.get_key_value` lets us
        //    return a reference to the typed PkgName the localdb owns
        //    rather than allocating a fresh one.
        for p in &entry.pkgnames {
            if let Some((stored_name, version)) = self.installed.get_key_value(&p.name) {
                return Some(InstalledCounterpart {
                    pkgname: stored_name,
                    version,
                    via: MatchedVia::Pkgname,
                });
            }
        }
        // 2. Replaces — explicit rename declaration.
        for r in &entry.replaces {
            let name = strip_version_constraint(r);
            if let Some((stored_name, version)) = self.installed.get_key_value(name) {
                return Some(InstalledCounterpart {
                    pkgname: stored_name,
                    version,
                    via: MatchedVia::Replaces,
                });
            }
        }
        // 3. Provides — collect every installed match across pkgname-scoped
        //    and pkgbase-level provides, preserving declaration order.
        //    De-dup by stored pkgname (a name appearing both scoped and at
        //    pkgbase-level is still one installed candidate).
        let mut provides_matches: Vec<(&PkgName, &Ver)> = Vec::new();
        let scoped_provs = entry.pkgnames.iter().flat_map(|p| &p.provides);
        for prov in scoped_provs.chain(entry.provides.iter()) {
            let name = strip_version_constraint(prov);
            if let Some((stored_name, version)) = self.installed.get_key_value(name)
                && !provides_matches.iter().any(|(n, _)| *n == stored_name)
            {
                provides_matches.push((stored_name, version.as_ver()));
            }
        }
        if provides_matches.len() > 1 {
            let alternatives: Vec<&PkgName> =
                provides_matches.iter().skip(1).map(|(n, _)| *n).collect();
            tracing::warn!(
                pkgbase = %entry.pkgbase,
                picked = %provides_matches[0].0,
                ?alternatives,
                "multiple installed pkgs match this pkgbase's provides; \
                 picking the first declared. Pass `--target <pkgname>` (or use \
                 the -Syu picker) to disambiguate.",
            );
        }
        if let Some(&(stored_name, version)) = provides_matches.first() {
            return Some(InstalledCounterpart {
                pkgname: stored_name,
                version,
                via: MatchedVia::Provides,
            });
        }
        None
    }

    /// pkgnames installed locally but not present in any syncdb (foreign).
    /// Returns owned typed `(PkgName, Version)` pairs so the caller can
    /// take its time without holding a borrow into the index.
    pub fn foreign(&self) -> Vec<(PkgName, Version)> {
        self.installed
            .iter()
            .filter(|(name, _)| !self.sync_versions.contains_key::<PkgName>(name))
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::Pkgname;

    /// Build an `IndexEntry` with the fields `counterpart` actually reads
    /// (pkgnames, replaces, provides). Everything else stays at default —
    /// `counterpart` ignores it.
    fn entry(
        pkgbase: &str,
        pkgnames: &[(&str, &[&str])],
        replaces: &[&str],
        provides: &[&str],
    ) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: pkgnames
                .iter()
                .map(|(n, provs)| Pkgname {
                    name: (*n).into(),
                    provides: provs.iter().map(|p| (*p).into()).collect(),
                    pkgdesc: None,
                })
                .collect(),
            replaces: replaces.iter().map(|s| (*s).into()).collect(),
            provides: provides.iter().map(|s| (*s).into()).collect(),
            ..IndexEntry::default()
        }
    }

    #[test]
    fn lookups_use_owned_hashes() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("vim".into(), "9.0-1".into());
        idx.sync_versions.insert("firefox".into(), "110.0-1".into());
        idx.sync_providers
            .insert("java-runtime".into(), vec!["jre-openjdk".into()]);

        assert!(idx.is_installed("vim"));
        assert!(!idx.is_installed("firefox"));
        assert!(idx.in_sync("firefox"));
        assert!(idx.in_sync("java-runtime"));
        assert!(!idx.in_sync("nonexistent"));
        assert_eq!(idx.installed_version("vim"), Some(Ver::new("9.0-1")));
        assert_eq!(idx.installed_version("firefox"), None);
        assert_eq!(idx.sync_version("firefox"), Some(Ver::new("110.0-1")));
        // Provides-only names carry no version of their own — only the
        // providing pkgname does.
        assert_eq!(idx.sync_version("java-runtime"), None);
    }

    #[test]
    fn size_lookups_read_owned_maps() {
        let mut idx = PacmanIndex::default();
        idx.installed_size.insert("vim".into(), 4_200_000);
        idx.sync_download_size.insert("firefox".into(), 78_000_000);
        // Cached repo pkg: download_size is 0, which is a real answer, not a
        // miss — `Some(0)`, never `None`.
        idx.sync_download_size.insert("glibc".into(), 0);

        assert_eq!(idx.installed_size(&"vim".into()), Some(4_200_000));
        assert_eq!(idx.installed_size(&"firefox".into()), None);
        assert_eq!(idx.sync_download_size(&"firefox".into()), Some(78_000_000));
        assert_eq!(idx.sync_download_size(&"glibc".into()), Some(0));
        assert_eq!(idx.sync_download_size(&"vim".into()), None);
    }

    #[test]
    fn sync_depends_returns_deps_or_empty() {
        let mut idx = PacmanIndex::default();
        idx.sync_versions.insert("firefox".into(), "110.0-1".into());
        idx.sync_depends
            .insert("firefox".into(), vec!["gtk3".into(), "libpulse".into()]);
        assert_eq!(
            idx.sync_depends(&"firefox".into()),
            &["gtk3".into(), "libpulse".into()] as &[PkgTarget],
        );
        // A pkgname with no recorded deps (or an unknown one) yields an empty
        // slice, never a panic.
        assert!(idx.sync_depends(&"bash".into()).is_empty());
    }

    #[test]
    fn foreign_excludes_sync_pkgs() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("vim".into(), "9.0-1".into());
        idx.installed.insert("paru-bin".into(), "2.0.0-1".into());
        idx.sync_versions.insert("vim".into(), "9.0-1".into());

        let mut foreign = idx.foreign();
        // Sort by pkgname only — `Version` has no `Ord` (vercmp is
        // PartialOrd; bytes-distinct + vercmp-equal corner case breaks
        // total order assumptions).
        foreign.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(foreign, vec![("paru-bin".into(), "2.0.0-1".into())]);
    }

    /// `resolve_concrete` is the single source of truth for "what would
    /// pacman actually install if I asked for this name?". Cover every
    /// branch: exact installed, installed-via-provides, exact sync, sync-
    /// via-provides, and unknown.
    #[test]
    fn resolve_concrete_orders_installed_before_sync() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("rust".into(), "1.80-1".into());
        idx.installed_providers
            .insert("cargo".into(), vec!["rust".into()]);
        idx.sync_versions.insert("pacman".into(), "6.1.0-1".into());
        idx.sync_providers
            .insert("libalpm.so".into(), vec!["pacman".into()]);
        idx.sync_versions.insert("rustup".into(), "1.27-1".into());
        // rustup also provides cargo, but rust (installed) must win.
        idx.sync_providers
            .entry("cargo".into())
            .or_default()
            .push("rustup".into());

        // `resolve_concrete` now returns the typed `&PkgName` (not `&str`);
        // construct the expected key once so the assertions read the same
        // way they would when comparing strings.
        let rust = PkgName::from("rust");
        let pacman = PkgName::from("pacman");
        assert_eq!(idx.resolve_concrete("rust"), Some((&rust, true)));
        assert_eq!(idx.resolve_concrete("cargo"), Some((&rust, true)));
        assert_eq!(idx.resolve_concrete("pacman"), Some((&pacman, false)));
        assert_eq!(idx.resolve_concrete("libalpm.so"), Some((&pacman, false)));
        assert_eq!(idx.resolve_concrete("nonexistent"), None);
    }

    /// Canonical case: pkgbase pkgname matches localdb directly. Provenance
    /// is `Pkgname` and the version comes from pacman.
    #[test]
    fn counterpart_matches_by_pkgname() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("foo".into(), "1.2.3-1".into());
        let e = entry("foo", &[("foo", &[])], &[], &[]);
        let c = idx.counterpart(&e).expect("foo is installed");
        assert_eq!(c.pkgname, "foo");
        assert_eq!(c.version, "1.2.3-1");
        assert_eq!(c.via, MatchedVia::Pkgname);
    }

    /// Split pkgbase with only one sibling installed: pkgname match still
    /// wins, and the matched name is the installed sibling — not the pkgbase.
    #[test]
    fn counterpart_picks_first_installed_split_sibling() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("bisq-cli".into(), "1.9-2".into());
        let e = entry(
            "bisq",
            &[
                ("bisq-desktop", &["bisq"]),
                ("bisq-cli", &[]),
                ("bisq-daemon", &[]),
            ],
            &[],
            &[],
        );
        let c = idx.counterpart(&e).expect("bisq-cli is installed");
        assert_eq!(c.pkgname, "bisq-cli");
        assert_eq!(c.version, "1.9-2");
        assert_eq!(c.via, MatchedVia::Pkgname);
    }

    /// `entry.replaces` ranks above `entry.provides` even when both could
    /// match: the explicit declaration is the more reliable signal.
    #[test]
    fn counterpart_prefers_replaces_over_provides() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("old-foo".into(), "0.9-1".into());
        let e = entry(
            "foo-ng",
            &[("foo-ng", &["old-foo"])], // also provides it
            &["old-foo"],                // and replaces it
            &[],
        );
        let c = idx.counterpart(&e).expect("old-foo is installed");
        assert_eq!(c.pkgname, "old-foo");
        assert_eq!(c.via, MatchedVia::Replaces);
    }

    /// Pkgname wins over both replaces and provides, even if the user has
    /// the legacy pkg installed alongside the new pkgbase (transitional).
    #[test]
    fn counterpart_prefers_pkgname_over_replaces_and_provides() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("foo-ng".into(), "2.0-1".into());
        idx.installed.insert("old-foo".into(), "0.9-1".into());
        let e = entry("foo-ng", &[("foo-ng", &["old-foo"])], &["old-foo"], &[]);
        let c = idx.counterpart(&e).expect("foo-ng is installed");
        assert_eq!(c.pkgname, "foo-ng");
        assert_eq!(c.via, MatchedVia::Pkgname);
    }

    /// The dotnet case: AUR pkgbase ships its own pkgname which `provides`
    /// the legacy name the user has installed. Matched via Provides; the
    /// version reflects the installed legacy pkg.
    #[test]
    fn counterpart_matches_pkgname_scoped_provides() {
        let mut idx = PacmanIndex::default();
        idx.installed
            .insert("dotnet-runtime-7.0".into(), "7.0.15-1".into());
        let e = entry(
            "dotnet-core-7.0-bin",
            &[(
                "dotnet-core-7.0-bin",
                &["dotnet-runtime-7.0", "dotnet-sdk-7.0"],
            )],
            &[],
            &[],
        );
        let c = idx
            .counterpart(&e)
            .expect("dotnet-runtime-7.0 is installed");
        assert_eq!(c.pkgname, "dotnet-runtime-7.0");
        assert_eq!(c.version, "7.0.15-1");
        assert_eq!(c.via, MatchedVia::Provides);
    }

    /// Pkgbase-level provides are inherited by every pkgname — match still
    /// resolves via the entry's top-level `provides` slot.
    #[test]
    fn counterpart_matches_pkgbase_level_provides() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("virt-name".into(), "3.0-1".into());
        let e = entry("foo", &[("foo", &[])], &[], &["virt-name"]);
        let c = idx.counterpart(&e).expect("virt-name is installed");
        assert_eq!(c.pkgname, "virt-name");
        assert_eq!(c.via, MatchedVia::Provides);
    }

    /// `provides = name=1.2` must strip the virtual version before lookup;
    /// the returned version comes from pacman (the installed pkgname's
    /// real version), not from the suffix.
    #[test]
    fn counterpart_strips_version_constraint_on_provides() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("libfoo".into(), "9.9-1".into());
        let e = entry("bar", &[("bar", &["libfoo=1.2"])], &[], &[]);
        let c = idx.counterpart(&e).expect("libfoo is installed");
        assert_eq!(c.pkgname, "libfoo");
        assert_eq!(c.version, "9.9-1"); // real version, not the virtual "1.2"
        assert_eq!(c.via, MatchedVia::Provides);
    }

    /// No pkgname / replaces / provides match anything installed → fresh
    /// install path, caller renders "install: …".
    #[test]
    fn counterpart_returns_none_when_nothing_installed() {
        let idx = PacmanIndex::default();
        let e = entry(
            "foo",
            &[("foo", &["virt"])],
            &["old-foo"],
            &["pkgbase-virt"],
        );
        assert!(idx.counterpart(&e).is_none());
    }

    /// Scoped provides (more specific) beats pkgbase-level provides when
    /// both could match.
    #[test]
    fn counterpart_prefers_scoped_provides_over_pkgbase_level() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("scoped".into(), "1-1".into());
        idx.installed.insert("toplevel".into(), "2-1".into());
        let e = entry("foo", &[("foo", &["scoped"])], &[], &["toplevel"]);
        let c = idx.counterpart(&e).expect("scoped is installed");
        assert_eq!(c.pkgname, "scoped");
    }

    // ──────────────────────────────────────────────────────────────────
    // counterpart_with_hint() — the dotnet-runtime regression cluster.
    // ──────────────────────────────────────────────────────────────────

    /// Two virtuals are installed; pkgbase declares both. Without a hint the
    /// first-declared provides wins. With a hint pointing at the second one,
    /// the lookup returns *that* one — the user's intent overrides
    /// declaration order.
    #[test]
    fn hint_steers_provides_match_to_user_intent() {
        let mut idx = PacmanIndex::default();
        idx.installed
            .insert("aspnet-runtime".into(), "10.0-1".into());
        idx.installed
            .insert("dotnet-runtime-7.0".into(), "7.0.20-1".into());
        // entry's `provides` declares aspnet-runtime first; without a hint
        // counterpart() picks that.
        let e = entry(
            "dotnet-core-7.0-bin",
            &[(
                "dotnet-core-7.0-bin",
                &["aspnet-runtime", "dotnet-runtime-7.0"],
            )],
            &[],
            &[],
        );
        let unhinted = idx.counterpart(&e).unwrap();
        assert_eq!(unhinted.pkgname, "aspnet-runtime");
        let hint = PkgName::from("dotnet-runtime-7.0");
        let hinted = idx.counterpart_with_hint(&e, Some(&hint)).unwrap();
        assert_eq!(hinted.pkgname, "dotnet-runtime-7.0");
        assert_eq!(hinted.via, MatchedVia::Provides);
    }

    /// Hint matches the canonical pkgname — provenance must be Pkgname,
    /// not Provides, even if the entry *also* declares a provides line for
    /// the same name (rare but real).
    #[test]
    fn hint_prefers_pkgname_provenance_over_provides() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("foo".into(), "1-1".into());
        // entry's pkgname is foo, AND it provides foo (a self-referential
        // provides=, which AUR doesn't reject).
        let e = entry("foo", &[("foo", &["foo"])], &[], &[]);
        let hint = PkgName::from("foo");
        let c = idx.counterpart_with_hint(&e, Some(&hint)).unwrap();
        assert_eq!(c.via, MatchedVia::Pkgname);
    }

    /// Hint matches a `replaces=` declaration. The pkgname rename case —
    /// user has the old name installed, AUR pkgbase declares it as replaced
    /// by the new pkgname.
    #[test]
    fn hint_returns_replaces_provenance() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("old-foo".into(), "0.9-1".into());
        let e = entry("foo-ng", &[("foo-ng", &[])], &["old-foo"], &[]);
        let hint = PkgName::from("old-foo");
        let c = idx.counterpart_with_hint(&e, Some(&hint)).unwrap();
        assert_eq!(c.pkgname, "old-foo");
        assert_eq!(c.via, MatchedVia::Replaces);
    }

    /// Hint is installed but the entry doesn't reference it (not a pkgname,
    /// not in replaces, not in provides). Fall back to the unhinted walk —
    /// otherwise a stale hint could silently nullify a real counterpart match.
    #[test]
    fn unmatched_hint_falls_back_to_unhinted_walk() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("unrelated".into(), "1-1".into());
        idx.installed.insert("real-target".into(), "2-1".into());
        let e = entry("foo", &[("foo", &["real-target"])], &[], &[]);
        let stale = PkgName::from("unrelated");
        let c = idx.counterpart_with_hint(&e, Some(&stale)).unwrap();
        assert_eq!(c.pkgname, "real-target");
        assert_eq!(c.via, MatchedVia::Provides);
    }

    /// Hint is not installed. Same fallback path — we only honour the hint
    /// when it identifies a real localdb entry to anchor on.
    #[test]
    fn non_installed_hint_falls_back_to_unhinted_walk() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("real-target".into(), "2-1".into());
        let e = entry("foo", &[("foo", &["real-target"])], &[], &[]);
        let missing = PkgName::from("never-installed");
        let c = idx.counterpart_with_hint(&e, Some(&missing)).unwrap();
        assert_eq!(c.pkgname, "real-target");
    }

    /// Hint with an explicit None — same behaviour as plain `counterpart()`.
    #[test]
    fn none_hint_matches_unhinted_counterpart() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("foo".into(), "1-1".into());
        let e = entry("foo", &[("foo", &[])], &[], &[]);
        assert_eq!(
            idx.counterpart_with_hint(&e, None).map(|c| c.pkgname),
            idx.counterpart(&e).map(|c| c.pkgname),
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // Multi-match Provides: behaviour exercise (warning is best-effort
    // tracing output, so the assertion is on the picked pkgname).
    // ──────────────────────────────────────────────────────────────────

    /// Two installed pkgs match the same pkgbase's `provides`. The unhinted
    /// walk picks the first declared (dotnet-core-7.0-bin declares
    /// aspnet-runtime before dotnet-runtime-7.0 in its PKGBUILD); the
    /// warning is emitted via `tracing::warn!` so the user sees an audit
    /// trail when the picked counterpart isn't the obviously right one.
    #[test]
    fn unhinted_multi_provides_match_picks_first_declared() {
        let mut idx = PacmanIndex::default();
        idx.installed
            .insert("aspnet-runtime".into(), "10.0-1".into());
        idx.installed
            .insert("dotnet-runtime-7.0".into(), "7.0.20-1".into());
        let e = entry(
            "dotnet-core-7.0-bin",
            &[(
                "dotnet-core-7.0-bin",
                &["aspnet-runtime", "dotnet-runtime-7.0"],
            )],
            &[],
            &[],
        );
        let c = idx.counterpart(&e).unwrap();
        assert_eq!(c.pkgname, "aspnet-runtime");
        assert_eq!(c.via, MatchedVia::Provides);
    }

    /// Scoped + pkgbase-level provides referencing the same installed pkg
    /// shouldn't count as two distinct matches — they're the same candidate
    /// declared twice. Tests dedup in the collection step.
    #[test]
    fn unhinted_dedup_scoped_and_pkgbase_level_provides() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("only-one".into(), "1-1".into());
        // The same name appears both pkgname-scoped AND pkgbase-level — the
        // collector should treat them as one match, not two, so no
        // multi-match warning fires.
        let e = entry("foo", &[("foo", &["only-one"])], &[], &["only-one"]);
        let c = idx.counterpart(&e).unwrap();
        assert_eq!(c.pkgname, "only-one");
    }
}
