//! PKGBUILD review UX: label by install/upgrade/reinstall, then show the diff.
//!
//! On upgrade, the diff is colored against the AUR commit whose `.SRCINFO`
//! declares the currently-installed version. Falls back to the full PKGBUILD
//! on fresh installs, reinstalls, and upgrades where no historic commit
//! matches (typical for VCS pkgbases whose `pkgver()` overrides the static
//! field at build time, or for installs older than the bounded history walk).
//!
//! Diff uses the bare mirror repo's object DB (not a `.git` inside the
//! worktree) — the build directory is just materialized files.

use crate::error::{Error, Result};
use crate::index::srcinfo;
use crate::mirror::MirrorRepo;
use crate::mirror::worktree::Worktree;
use crate::names::PkgBase;
use crate::pacman::alpm_db::{InstalledCounterpart, MatchedVia};
use crate::ui;
use crate::version::Ver;
use gix::ObjectId;
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, info_span, instrument};

// History walk bound is now configurable via
// `Config::review_history_scan_max` — plumbed through `cmd_install` →
// `prepare_one` → `review::review` → `find_installed_commit`. Old static
// `MAX_HISTORY_SCAN = 64` constant removed; the fallback notes carry the
// runtime value so the user always knows what was actually searched.

/// `git diff --unified=<N>` value for "view full diff" — large enough to
/// blanket any realistic PKGBUILD so every unchanged line is shown.
const FULL_DIFF_CONTEXT: u32 = 99_999;

/// What the user decided about this pkgbase. `Aborted` short-circuits the
/// whole pipeline (propagated as [`Error::UserAbort`] by the caller), so it
/// isn't a variant here — only "include it" vs "drop it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// User approved: include in the upcoming build batch.
    Approved,
    /// User chose "skip": drop this pkgbase from the build batch but keep
    /// reviewing the rest.
    Skipped,
}

/// Drive the review prompt loop for one pkgbase.
///
/// `counterpart` is the pacman-localdb pkg this build will displace — `None`
/// for a fresh install, otherwise carrying the installed pkgname, its
/// version, and how the AUR entry referenced it (pkgname / replaces /
/// provides). `new_ver` is the version the AUR index reports for this
/// pkgbase.
#[instrument(skip(mirror, wt), fields(pkgbase = %pkgbase))]
pub fn review(
    mirror: &MirrorRepo,
    pkgbase: &PkgBase,
    new_ver: &Ver,
    counterpart: Option<&InstalledCounterpart<'_>>,
    wt: &Worktree,
    history_scan_max: usize,
    noconfirm: bool,
) -> Result<Outcome> {
    if noconfirm {
        // Carry counterpart provenance in the trace so non-interactive runs
        // (container smoke tests, CI, scripts) can still verify that the
        // installed pkg was correctly resolved across pkgname/replaces/provides.
        // Without this the noconfirm path is opaque — show() never renders.
        // `?` (Debug) formats `Option<…>` as `Some(…)` / `None` so the absent
        // case is grep-distinguishable from a present-but-empty one.
        info!(
            %pkgbase,
            %new_ver,
            installed = ?counterpart.map(|c| c.pkgname),
            installed_version = ?counterpart.map(|c| c.version),
            via = ?counterpart.map(|c| c.via),
            "auto-proceeding (noconfirm)"
        );
        return Ok(Outcome::Approved);
    }

    // Initial render runs once — the loop body dispatches user actions and
    // re-renders only what the user asked for, so picking "view PKGBUILD"
    // isn't immediately clobbered by re-printing the diff above the prompt.
    let base = show(mirror, pkgbase, new_ver, counterpart, wt, history_scan_max)?;

    let actions = menu_actions(base.is_some());

    loop {
        match prompt_action(pkgbase, &actions)? {
            Action::Approve => return Ok(Outcome::Approved),
            Action::ViewPkgbuild => show_pkgbuild(wt)?,
            Action::ViewDiff => show_diff(
                mirror,
                wt,
                base.expect("only present when a diff base was found"),
                Some(FULL_DIFF_CONTEXT),
            )?,
            Action::Edit => edit_pkgbuild(wt)?,
            Action::Skip => return Ok(Outcome::Skipped),
            Action::Abort => return Err(Error::UserAbort),
        }
    }
}

/// Print the default review view and return the diff base, if any, so the
/// caller can decide whether to offer "view full diff".
fn show(
    mirror: &MirrorRepo,
    pkgbase: &PkgBase,
    new_ver: &Ver,
    counterpart: Option<&InstalledCounterpart<'_>>,
    wt: &Worktree,
    history_scan_max: usize,
) -> Result<Option<ObjectId>> {
    ui::step(&header(pkgbase, new_ver, counterpart));

    // Anything that isn't a real upgrade (fresh install, reinstall of the
    // canonical pkgname) has no historic version to diff against — show the
    // full PKGBUILD.
    let Some(installed) = upgrade_base_version(new_ver, counterpart) else {
        show_pkgbuild(wt)?;
        return Ok(None);
    };

    match find_installed_commit(mirror, wt.head_oid, installed, history_scan_max)? {
        HistorySearch::Found(base) => {
            show_diff(mirror, wt, base, None)?;
            Ok(Some(base))
        }
        outcome => {
            let c =
                counterpart.expect("upgrade_base_version is Some only when counterpart is Some");
            ui::note(&fallback_note(pkgbase, c, outcome));
            show_pkgbuild(wt)?;
            Ok(None)
        }
    }
}

/// Label the review screen. Six cases, derived from `counterpart`:
///
/// | counterpart                              | label                                                     |
/// | ---------------------------------------- | --------------------------------------------------------- |
/// | `None`                                   | `install: {pkgbase} {new}`                                |
/// | `Some(via=Pkgname, ver==new)`            | `reinstall: {pkgbase} {new}`                              |
/// | `Some(via=Pkgname)`                      | `upgrade: {pkgbase} {ver} → {new}`                        |
/// | `Some(via=Replaces)`                     | `upgrade: {pkgbase} {ver} → {new}  [replaces {name}]`     |
/// | `Some(via=Provides, name==pkgbase)`      | `upgrade: {pkgbase} {ver} → {new}`                        |
/// | `Some(via=Provides)`                     | `upgrade: {pkgbase} {ver} → {new}  [provides {name}]`     |
///
/// The `[…]` annotation appears exactly when the user's installed pkgname
/// differs from the build target — that's the moment they need to know
/// "you're not upgrading literally the thing you have installed; this is a
/// transition." A `Provides` match against a name equal to the pkgbase
/// (degenerate) doesn't get the annotation because there's nothing to
/// distinguish for the reader. `Reinstall` is reserved for `Pkgname` matches:
/// a `Provides`/`Replaces` match with coincidentally-equal versions is still
/// a transition, not a reinstall, and showing it as a diff is more honest.
fn header(
    pkgbase: &PkgBase,
    new_ver: &Ver,
    counterpart: Option<&InstalledCounterpart<'_>>,
) -> String {
    let Some(c) = counterpart else {
        return format!("install: {pkgbase} {new_ver}");
    };
    if c.via == MatchedVia::Pkgname && c.version == new_ver {
        return format!("reinstall: {pkgbase} {new_ver}");
    }
    let head = format!("upgrade: {pkgbase} {} → {new_ver}", c.version);
    match c.via {
        MatchedVia::Pkgname => head,
        MatchedVia::Replaces => format!("{head}  [replaces {}]", c.pkgname),
        MatchedVia::Provides if pkgbase.matches_pkgname(c.pkgname) => head,
        MatchedVia::Provides => format!("{head}  [provides {}]", c.pkgname),
    }
}

/// The installed version to feed [`find_installed_commit`] when this is a
/// real upgrade, or `None` for install / canonical-reinstall. Pulled out so
/// `show` keeps a single dispatch and so the rule is unit-testable.
///
/// Provides/Replaces matches with `ver == new_ver` still count as upgrades
/// here: the history walk lets us *try* to show a diff (case A in the design
/// table — pkgname rename inside the same pkgbase). If the walk misses, we
/// fall back to full PKGBUILD with a provenance-aware note.
fn upgrade_base_version<'a>(
    new_ver: &Ver,
    counterpart: Option<&'a InstalledCounterpart<'_>>,
) -> Option<&'a Ver> {
    let c = counterpart?;
    match c.via {
        MatchedVia::Pkgname if c.version == new_ver => None,
        _ => Some(c.version),
    }
}

/// The "no diff base found" note. Two axes drive the wording:
///
///   * Provenance ([`MatchedVia`]): "matches installed X" (pkgname tier
///     — same lineage) vs "produced installed X" (replaces/provides —
///     lineage transition). Same as before.
///   * Walk outcome ([`HistorySearch`]): branch-exhausted vs bound-hit.
///     Different actionable advice — bumping `review_history_scan_max`
///     helps for bound-hit only.
///
/// The dotnet-runtime-7.0 case: install came from the (now-EOL'd)
/// official `extra/` repo (`7.0.20.sdk120-2`), AUR pkg
/// `dotnet-core-7.0-bin` provides the same virtual but is an independent
/// lineage (`sdk410`, `sdk406`, …) whose 6 commits walk to root well
/// under the 256 bound. `NotInLineage` arm explains that and steers the
/// user away from a useless bound bump.
fn fallback_note(
    pkgbase: &PkgBase,
    c: &InstalledCounterpart<'_>,
    outcome: HistorySearch,
) -> String {
    let verb = match c.via {
        MatchedVia::Pkgname => "matches",
        MatchedVia::Replaces | MatchedVia::Provides => "produced",
    };
    let coda = match outcome {
        HistorySearch::Found(_) => "",
        HistorySearch::NotInLineage { .. } => {
            // Branch fully walked; bumping won't help. Most common cause for
            // provides-tier mismatches: install came from a different source
            // (former extra/community repo, a renamed pkgbase, a different
            // AUR pkg providing the same virtual).
            match c.via {
                MatchedVia::Pkgname => " — this version isn't in the pkgbase's git history",
                MatchedVia::Replaces | MatchedVia::Provides => {
                    " — the installed pkg likely came from a different source \
                     (an EOL'd official repo, a renamed pkgbase, or a sibling \
                     AUR pkg that also declares this provides=)"
                }
            }
        }
        HistorySearch::BoundExceeded { .. } => {
            " — raise `review_history_scan_max` in config.toml if the install \
             predates the search bound"
        }
    };
    let scope = match outcome {
        HistorySearch::Found(_) | HistorySearch::BoundExceeded { .. } => {
            // Either we found it (no note rendered) or we hit the bound;
            // in the bound case the user already knows N from the bound
            // value, and we surface it via the coda. Keep the prefix
            // generic for both code paths.
            format!("last {} of {pkgbase}", history_scan_bound_for(outcome))
        }
        HistorySearch::NotInLineage { walked } => {
            // Branch-exhausted: name the actual walk depth so the user
            // sees "the branch only has N commits" — no ambiguity.
            format!("{walked} ancestor(s) of {pkgbase}")
        }
    };
    format!(
        "no AUR commit in the {scope} {verb} installed {} ({}); showing full PKGBUILD{coda}",
        c.pkgname, c.version,
    )
}

/// Numeric bound to display in the `scope` prefix. `NotInLineage`
/// callers use `walked` instead; this is only consulted for
/// `BoundExceeded`/`Found`.
const fn history_scan_bound_for(outcome: HistorySearch) -> usize {
    match outcome {
        HistorySearch::BoundExceeded { bound } => bound,
        HistorySearch::Found(_) | HistorySearch::NotInLineage { .. } => 0,
    }
}

fn show_pkgbuild(wt: &Worktree) -> Result<()> {
    let text = std::fs::read_to_string(wt.path.join("PKGBUILD"))?;
    print!("{}", highlight::pkgbuild(&text));
    Ok(())
}

fn edit_pkgbuild(wt: &Worktree) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let pkgbuild = wt.path.join("PKGBUILD");
    debug!(editor, file = %pkgbuild.display(), "launching editor");
    let status = Command::new(editor).arg(&pkgbuild).status()?;
    if !status.success() {
        return Err(Error::Build(format!("editor exited {:?}", status.code())));
    }
    Ok(())
}

/// Show a line-diff of `PKGBUILD` between `base` and the freshly-materialized
/// worktree's commit. Delegates to the user's `git diff` (so their configured
/// pager / external differ — delta, diff-so-fancy, difftastic, etc. — kicks
/// in automatically when stdout is a TTY). Listing every other changed path
/// is left to the user — they have a real linked worktree where plain
/// `git diff` works.
///
/// `context = None` lets git apply its default (and any user `diff.context`
/// override); `Some(n)` forces a `--unified=n` cap — used by "view full diff"
/// to drop the unchanged-line elision so reviewers can see every line.
fn show_diff(
    mirror: &MirrorRepo,
    wt: &Worktree,
    base: ObjectId,
    context: Option<u32>,
) -> Result<()> {
    // `git diff` exits 0 when there are no differences and 1 when there are
    // — both are success. Any other status (or a spawn failure) is a real
    // error worth surfacing. The diff streams to the terminal (a pager), so it
    // runs directly rather than through `git::run`; the span keeps it visible
    // in the trace alongside the captured-output git calls.
    let _span = info_span!("git", subcommand = "diff").entered();
    let status = diff_command(&mirror.path, base, wt.head_oid, context)
        .status()
        .map_err(|e| Error::other(format!("spawn git diff: {e}")))?;
    match status.code() {
        Some(0 | 1) => Ok(()),
        Some(c) => Err(Error::other(format!("git diff exited {c}"))),
        None => Err(Error::other("git diff terminated by signal".to_owned())),
    }
}

/// Build the `git diff` command used for review. Split out so tests can
/// assert the argument shape without spawning git.
fn diff_command(
    mirror_path: &Path,
    base: ObjectId,
    head: ObjectId,
    context: Option<u32>,
) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(mirror_path).arg("diff");
    if let Some(n) = context {
        cmd.arg(format!("--unified={n}"));
    }
    cmd.arg(base.to_string())
        .arg(head.to_string())
        .args(["--", "PKGBUILD"]);
    cmd
}

/// One selectable action on the review prompt.
///
/// The prompt is plain text, not an interactive menu: each action advertises
/// a single hotkey (the parenthesized letter in its [`Self::label`]) and the
/// user types that letter. `Approve` is first so a bare enter — or a
/// piped/closed stdin — falls back to it, matching the old `Select` default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Include this pkgbase in the build batch.
    Approve,
    /// Print the full PKGBUILD source.
    ViewPkgbuild,
    /// Show the full-context diff against the installed version's commit.
    /// Offered only when a diff base was found.
    ViewDiff,
    /// Open the PKGBUILD in `$EDITOR`.
    Edit,
    /// Drop this pkgbase but keep reviewing the rest.
    Skip,
    /// Abort the whole review pass.
    Abort,
}

impl Action {
    /// The key that selects this action, matched case-insensitively against
    /// the first non-space character the user types.
    const fn key(self) -> char {
        match self {
            Self::Approve => 'a',
            Self::ViewPkgbuild => 'p',
            Self::ViewDiff => 'd',
            Self::Edit => 'e',
            Self::Skip => 's',
            Self::Abort => 'q',
        }
    }

    /// The prompt label, with the hotkey parenthesized (`(a)pprove`).
    const fn label(self) -> &'static str {
        match self {
            Self::Approve => "(a)pprove",
            Self::ViewPkgbuild => "view (p)kgbuild",
            Self::ViewDiff => "view full (d)iff",
            Self::Edit => "(e)dit",
            Self::Skip => "(s)kip",
            Self::Abort => "(q)uit",
        }
    }
}

/// Review actions in display (and default-fallback) order. "view full diff"
/// only appears when there is a diff base to render against — on
/// install/reinstall the initial view is already the whole PKGBUILD, so a
/// full-context diff would be redundant.
fn menu_actions(has_diff: bool) -> Vec<Action> {
    let mut actions = vec![Action::Approve, Action::ViewPkgbuild];
    if has_diff {
        actions.push(Action::ViewDiff);
    }
    actions.extend_from_slice(&[Action::Edit, Action::Skip, Action::Abort]);
    actions
}

/// Resolve a raw input line to an action. An empty line (bare enter) picks
/// the default — the first action, `Approve` — mirroring the old `Select`'s
/// `default(0)`. An unrecognized key yields `None` so the caller re-prompts.
fn parse_action(line: &str, actions: &[Action]) -> Option<Action> {
    match line.trim().chars().next() {
        None => actions.first().copied(),
        Some(c) => {
            let c = c.to_ascii_lowercase();
            actions.iter().copied().find(|a| a.key() == c)
        }
    }
}

/// Render the plain-text action prompt and block for one line of input,
/// re-prompting on an unrecognized key. A closed/piped stdin (EOF) falls back
/// to the default action so a non-interactive caller can't spin. The prompt
/// goes to stderr (unbuffered, so no flush) to keep stdout clean for the
/// PKGBUILD/diff content a caller might pipe.
fn prompt_action(pkgbase: &PkgBase, actions: &[Action]) -> Result<Action> {
    let labels = actions
        .iter()
        .map(|a| a.label())
        .collect::<Vec<_>>()
        .join(", ");
    loop {
        eprint!("[{pkgbase}] review — {labels}: ");
        let mut line = String::new();
        let read = std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| Error::other(format!("prompt: {e}")))?;
        if read == 0 {
            // EOF: behave like the enter-default rather than looping forever.
            return Ok(actions[0]);
        }
        if let Some(action) = parse_action(&line, actions) {
            return Ok(action);
        }
        eprintln!("unrecognized choice — type one of the parenthesized letters");
    }
}

/// Outcome of [`find_installed_commit`]'s history walk.
///
/// Three distinct cases — `fallback_note` keys its phrasing off this so
/// the user gets actionable advice. Without the distinction "no AUR
/// commit matched" reads the same whether we walked 6 commits to the
/// branch root or topped out at 256 with thousands more below.
///
/// The motivating example: an installed `dotnet-runtime-7.0` came from
/// the (now-EOL'd) `extra/` repo, version `7.0.20.sdk120-2`. The user
/// still has that installed; the AUR pkg `dotnet-core-7.0-bin` provides
/// the same virtual but is an independent lineage whose versions
/// (`sdk410`, `sdk406`, …) never aligned with the official's `sdkXXX`
/// numbering. The walk reaches root at ~6 commits — `NotInLineage` —
/// and bumping `review_history_scan_max` would be wasted effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistorySearch {
    /// Matching commit's OID, ready for `git diff`.
    Found(ObjectId),
    /// Walked all the way to the branch's root commit without matching.
    /// The version isn't in this pkgbase's lineage — raising
    /// `review_history_scan_max` won't help. `walked` is the number of
    /// commits actually inspected (useful for the fallback note).
    NotInLineage { walked: usize },
    /// Stopped at `history_scan_max` without matching. The commit may
    /// exist further back; raising the bound is the suggested fix.
    BoundExceeded { bound: usize },
}

/// Walk the AUR branch back from `head_oid` looking for the commit whose
/// `.SRCINFO` declares `installed_ver`.
///
/// Returns a [`HistorySearch`] tag distinguishing match / branch-exhausted /
/// bound-hit so the caller can pick wording that's actually true.
///
/// VCS pkgbases never match here because their static pkgver is overridden
/// by `pkgver()` at build time. Very stale installs may sit further back
/// than the bound; raising `Config::review_history_scan_max` is the
/// targeted knob for that case.
///
/// Uses `.SRCINFO` rather than parsing `PKGBUILD` ourselves: the AUR ships
/// the post-bash-expansion `.SRCINFO` alongside every PKGBUILD, and the
/// existing [`srcinfo::parse`] already turns it into an [`IndexEntry`] —
/// the same code path the rkyv index uses.
///
/// `pub` for integration tests (`tests/review_diff_history.rs`).
pub fn find_installed_commit(
    mirror: &MirrorRepo,
    head_oid: ObjectId,
    installed_ver: &Ver,
    history_scan_max: usize,
) -> Result<HistorySearch> {
    let head = mirror
        .repo
        .find_commit(head_oid)
        .map_err(|e| Error::Gix(format!("find_commit {head_oid}: {e}")))?;
    let walk = head
        .ancestors()
        .first_parent_only()
        .all()
        .map_err(|e| Error::Gix(format!("ancestors {head_oid}: {e}")))?;
    let mut walked = 0usize;
    for info in walk.take(history_scan_max) {
        walked += 1;
        let info = info.map_err(|e| Error::Gix(format!("walk: {e}")))?;
        let tree = info
            .object()
            .map_err(|e| Error::Gix(format!("walk object {}: {e}", info.id)))?
            .tree()
            .map_err(|e| Error::Gix(format!("walk tree {}: {e}", info.id)))?;
        let Some(text) = read_blob(mirror, &tree, ".SRCINFO")? else {
            continue;
        };
        let Ok(entry) = srcinfo::parse(&text) else {
            continue;
        };
        if entry.version() == installed_ver {
            return Ok(HistorySearch::Found(info.id));
        }
    }
    // If we consumed fewer commits than the bound, the ancestors iterator
    // ran out — the entire branch was walked. Otherwise we hit the bound
    // and there's potentially more history below.
    if walked < history_scan_max {
        Ok(HistorySearch::NotInLineage { walked })
    } else {
        Ok(HistorySearch::BoundExceeded {
            bound: history_scan_max,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Action, FULL_DIFF_CONTEXT, HistorySearch, diff_command, fallback_note, header,
        menu_actions, parse_action, upgrade_base_version,
    };
    use crate::names::{PkgBase, PkgName};
    use crate::pacman::alpm_db::{InstalledCounterpart, MatchedVia};
    use crate::version::{Ver, Version};
    use gix::ObjectId;
    use gix::hash::Kind;
    use std::path::Path;

    /// Default search bound used in the `fallback_note` assertions — the
    /// constant from `Config::review_history_scan_max`'s default. Tests
    /// that exercise the bound itself use literal values inline.
    const TEST_HISTORY_SCAN: usize = 256;

    /// Owning fixture for an `InstalledCounterpart`. `InstalledCounterpart`
    /// borrows its `pkgname`, so tests need a stable address — `Fixture`
    /// holds the `PkgName` value and hands out an `InstalledCounterpart`
    /// borrowing into itself. One `let f = fx(...)` per test, then `f.cp()`.
    struct Fixture {
        pkgname: PkgName,
        version: Version,
        via: MatchedVia,
    }

    impl Fixture {
        fn cp(&self) -> InstalledCounterpart<'_> {
            InstalledCounterpart {
                pkgname: &self.pkgname,
                version: self.version.as_ver(),
                via: self.via,
            }
        }
    }

    fn fx(pkgname: &str, version: &str, via: MatchedVia) -> Fixture {
        Fixture {
            pkgname: PkgName::from(pkgname),
            version: Version::from(version),
            via,
        }
    }

    /// Pkgbase literal helper for the `header` / `fallback_note` signature.
    fn pb(s: &str) -> PkgBase {
        PkgBase::from(s)
    }

    /// `&Ver` literal helper for the `header`/`upgrade_base_version`/...
    /// signatures — `v("1.0-1")` reads tersely at the test call site.
    fn v(s: &str) -> &Ver {
        Ver::new(s)
    }

    fn args_of(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn menu_omits_full_diff_when_no_base() {
        assert_eq!(
            menu_actions(false),
            [
                Action::Approve,
                Action::ViewPkgbuild,
                Action::Edit,
                Action::Skip,
                Action::Abort
            ]
        );
    }

    #[test]
    fn menu_inserts_full_diff_after_view_pkgbuild() {
        assert_eq!(
            menu_actions(true),
            [
                Action::Approve,
                Action::ViewPkgbuild,
                Action::ViewDiff,
                Action::Edit,
                Action::Skip,
                Action::Abort
            ]
        );
    }

    #[test]
    fn menu_default_action_is_approve() {
        // A bare enter / EOF falls back to the first action; it must stay
        // "approve" so hitting enter matches the pre-refactor `Select` default
        // and never picks a destructive or surprising action.
        assert_eq!(menu_actions(false)[0], Action::Approve);
        assert_eq!(menu_actions(true)[0], Action::Approve);
    }

    #[test]
    fn action_hotkeys_are_unique() {
        let actions = menu_actions(true);
        let mut keys: Vec<char> = actions.iter().map(|a| a.key()).collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "duplicate hotkey in {actions:?}");
    }

    #[test]
    fn action_label_advertises_its_hotkey() {
        for a in menu_actions(true) {
            assert!(
                a.label().contains(&format!("({})", a.key())),
                "label {:?} should parenthesize its hotkey {:?}",
                a.label(),
                a.key()
            );
        }
    }

    #[test]
    fn parse_action_empty_line_is_the_default() {
        let actions = menu_actions(true);
        assert_eq!(parse_action("", &actions), Some(Action::Approve));
        assert_eq!(parse_action("   \n", &actions), Some(Action::Approve));
    }

    #[test]
    fn parse_action_matches_hotkey_case_insensitively() {
        let actions = menu_actions(true);
        assert_eq!(parse_action("s", &actions), Some(Action::Skip));
        assert_eq!(parse_action("S\n", &actions), Some(Action::Skip));
        assert_eq!(parse_action("d", &actions), Some(Action::ViewDiff));
        // A whole word still keys off its first letter.
        assert_eq!(parse_action("quit", &actions), Some(Action::Abort));
    }

    #[test]
    fn parse_action_unrecognized_key_is_none() {
        let actions = menu_actions(true);
        assert_eq!(parse_action("z", &actions), None);
    }

    #[test]
    fn parse_action_diff_key_absent_without_base() {
        // 'd' selects "view full diff" only when that action is on offer.
        let actions = menu_actions(false);
        assert_eq!(parse_action("d", &actions), None);
    }

    #[test]
    fn diff_command_default_omits_unified_flag() {
        // No `--unified=N` means git falls back to its default (and the user's
        // `diff.context` config), matching the pre-refactor behavior.
        let zero = ObjectId::null(Kind::Sha1);
        let cmd = diff_command(Path::new("/tmp/m"), zero, zero, None);
        let args = args_of(&cmd);
        assert!(
            !args.iter().any(|a| a.starts_with("--unified")),
            "expected no --unified flag, got {args:?}"
        );
    }

    #[test]
    fn diff_command_full_context_passes_large_unified() {
        let zero = ObjectId::null(Kind::Sha1);
        let cmd = diff_command(Path::new("/tmp/m"), zero, zero, Some(FULL_DIFF_CONTEXT));
        let args = args_of(&cmd);
        assert!(
            args.iter().any(|a| a == "--unified=99999"),
            "expected --unified=99999, got {args:?}"
        );
    }

    #[test]
    fn diff_command_targets_pkgbuild_in_mirror() {
        let zero = ObjectId::null(Kind::Sha1);
        let cmd = diff_command(Path::new("/var/lib/gitaur/foo.git"), zero, zero, None);
        let args = args_of(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("-C"));
        assert_eq!(
            args.get(1).map(String::as_str),
            Some("/var/lib/gitaur/foo.git")
        );
        assert_eq!(args.get(2).map(String::as_str), Some("diff"));
        // Pathspec separator and target file at the tail.
        let tail: Vec<&str> = args.iter().rev().take(2).map(String::as_str).collect();
        assert_eq!(tail, ["PKGBUILD", "--"]);
    }

    // ──────────────────────────────────────────────────────────────────
    // header() — six rows from the design table in the doc-comment above.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn header_install_when_no_counterpart() {
        assert_eq!(header(&pb("foo"), v("1.0-1"), None), "install: foo 1.0-1");
    }

    #[test]
    fn header_reinstall_when_canonical_pkgname_at_same_version() {
        let f = fx("foo", "1.0-1", MatchedVia::Pkgname);
        assert_eq!(
            header(&pb("foo"), v("1.0-1"), Some(&f.cp())),
            "reinstall: foo 1.0-1"
        );
    }

    #[test]
    fn header_upgrade_canonical_pkgname() {
        let f = fx("foo", "1.0-1", MatchedVia::Pkgname);
        assert_eq!(
            header(&pb("foo"), v("1.1-1"), Some(&f.cp())),
            "upgrade: foo 1.0-1 → 1.1-1"
        );
    }

    /// Split pkgbase: installed pkgname != pkgbase but still matched by
    /// pkgname. The pkgbase is what we build, so the header keeps it without
    /// further annotation — the sibling identity is just a detail.
    #[test]
    fn header_upgrade_split_sibling_no_annotation() {
        let f = fx("bisq-cli", "1.9-1", MatchedVia::Pkgname);
        assert_eq!(
            header(&pb("bisq"), v("2.0-1"), Some(&f.cp())),
            "upgrade: bisq 1.9-1 → 2.0-1"
        );
    }

    #[test]
    fn header_upgrade_via_replaces_annotates() {
        let f = fx("old-foo", "0.9-1", MatchedVia::Replaces);
        assert_eq!(
            header(&pb("foo-ng"), v("1.0-1"), Some(&f.cp())),
            "upgrade: foo-ng 0.9-1 → 1.0-1  [replaces old-foo]"
        );
    }

    #[test]
    fn header_upgrade_via_provides_annotates() {
        let f = fx("dotnet-runtime-7.0", "7.0.15-1", MatchedVia::Provides);
        assert_eq!(
            header(&pb("dotnet-core-7.0-bin"), v("7.0.20-2"), Some(&f.cp())),
            "upgrade: dotnet-core-7.0-bin 7.0.15-1 → 7.0.20-2  [provides dotnet-runtime-7.0]"
        );
    }

    /// Degenerate case: provides match where the installed name equals the
    /// pkgbase. No annotation, since there's nothing to disambiguate.
    #[test]
    fn header_upgrade_via_provides_omits_annotation_when_name_equals_pkgbase() {
        let f = fx("foo", "1.0-1", MatchedVia::Provides);
        assert_eq!(
            header(&pb("foo"), v("1.1-1"), Some(&f.cp())),
            "upgrade: foo 1.0-1 → 1.1-1"
        );
    }

    /// Reinstall classification is reserved for `Pkgname` matches —
    /// coincidental version equality across a provides/replaces transition
    /// is still a transition, and we'd rather show a diff (which falls back
    /// to full PKGBUILD when history misses) than mislabel it as reinstall.
    #[test]
    fn header_does_not_call_provides_transition_a_reinstall() {
        let f = fx("old-foo", "1.0-1", MatchedVia::Provides);
        assert_eq!(
            header(&pb("foo-ng"), v("1.0-1"), Some(&f.cp())),
            "upgrade: foo-ng 1.0-1 → 1.0-1  [provides old-foo]"
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // upgrade_base_version() — controls whether we attempt a diff at all.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn upgrade_base_none_when_no_counterpart() {
        assert_eq!(upgrade_base_version(v("1.0-1"), None), None);
    }

    #[test]
    fn upgrade_base_none_for_canonical_reinstall() {
        let f = fx("foo", "1.0-1", MatchedVia::Pkgname);
        assert_eq!(upgrade_base_version(v("1.0-1"), Some(&f.cp())), None);
    }

    #[test]
    fn upgrade_base_some_for_canonical_upgrade() {
        let f = fx("foo", "1.0-1", MatchedVia::Pkgname);
        assert_eq!(
            upgrade_base_version(v("1.1-1"), Some(&f.cp())),
            Some(v("1.0-1"))
        );
    }

    /// Even at equal version, a provides/replaces match should attempt a
    /// diff — the history walk decides the outcome.
    #[test]
    fn upgrade_base_some_for_provides_even_at_same_version() {
        let f = fx("old", "1.0-1", MatchedVia::Provides);
        assert_eq!(
            upgrade_base_version(v("1.0-1"), Some(&f.cp())),
            Some(v("1.0-1"))
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // fallback_note() — phrasing depends on provenance AND walk outcome.
    // ──────────────────────────────────────────────────────────────────

    /// Bound-hit (`BoundExceeded`): the commit may exist past the bound
    /// — the note should point the user at the config knob.
    #[test]
    fn fallback_note_bound_exceeded_recommends_config_bump() {
        let f = fx("foo", "0.5-1", MatchedVia::Pkgname);
        let note = fallback_note(
            &pb("foo"),
            &f.cp(),
            HistorySearch::BoundExceeded {
                bound: TEST_HISTORY_SCAN,
            },
        );
        assert!(
            note.contains(&format!("last {TEST_HISTORY_SCAN} of foo")),
            "bound-exceeded note should name the bound: {note}"
        );
        assert!(
            note.contains("review_history_scan_max"),
            "bound-exceeded note should point at the config knob: {note}"
        );
        assert!(note.contains("foo (0.5-1)"));
    }

    /// Branch-exhausted (`NotInLineage`): bumping the bound is useless;
    /// the note must NOT suggest it, and SHOULD name the actual walk
    /// depth so "the branch only has N commits" is visible. The
    /// dotnet-runtime-7.0 case the user actually hit.
    #[test]
    fn fallback_note_not_in_lineage_explains_alternate_source() {
        let f = fx(
            "dotnet-runtime-7.0",
            "7.0.20.sdk120-2",
            MatchedVia::Provides,
        );
        let note = fallback_note(
            &pb("dotnet-core-7.0-bin"),
            &f.cp(),
            HistorySearch::NotInLineage { walked: 6 },
        );
        assert!(
            note.contains("6 ancestor(s) of dotnet-core-7.0-bin"),
            "not-in-lineage note should name the actual walk depth: {note}"
        );
        assert!(
            !note.contains("review_history_scan_max"),
            "not-in-lineage note must NOT suggest raising the bound: {note}"
        );
        assert!(
            note.contains("different source"),
            "not-in-lineage provides note should mention the alternate-source explanation: {note}"
        );
        assert!(note.contains("dotnet-runtime-7.0 (7.0.20.sdk120-2)"));
    }

    /// Pkgname-tier branch-exhausted: same-lineage match failed even
    /// after walking the whole branch. Note should be terse — "this
    /// version isn't in the pkgbase's history" — without the
    /// provides-tier alternate-source spiel which doesn't apply.
    #[test]
    fn fallback_note_pkgname_not_in_lineage() {
        let f = fx("foo", "0.5-1", MatchedVia::Pkgname);
        let note = fallback_note(
            &pb("foo"),
            &f.cp(),
            HistorySearch::NotInLineage { walked: 3 },
        );
        assert!(note.contains("3 ancestor(s) of foo"));
        assert!(note.contains("isn't in the pkgbase's git history"));
        assert!(!note.contains("review_history_scan_map"));
    }
}

fn read_blob(mirror: &MirrorRepo, tree: &gix::Tree<'_>, name: &str) -> Result<Option<String>> {
    let Some(entry) = tree.find_entry(name) else {
        return Ok(None);
    };
    let oid = entry.oid().to_owned();
    let blob = mirror
        .repo
        .find_object(oid)
        .map_err(|e| Error::Gix(format!("find {name} blob: {e}")))?;
    Ok(Some(
        String::from_utf8_lossy(blob.data.as_slice()).into_owned(),
    ))
}

mod highlight {
    //! Bash syntax coloring for the PKGBUILD review screen, via `syntect`'s
    //! bundled Sublime grammar (same grammar `bat` uses for `.sh`/PKGBUILD).
    //!
    //! Loaded lazily — the bundled `SyntaxSet` costs ~100 ms to parse on first
    //! use, then is cached for the rest of the process. Any failure (theme
    //! missing, grammar unloadable, per-line highlight error) falls back to
    //! plain text rather than aborting review.
    use crate::ui;
    use std::sync::OnceLock;
    use syntect::easy::HighlightLines;
    use syntect::highlighting::{Theme, ThemeSet};
    use syntect::parsing::SyntaxSet;
    use syntect::util::{LinesWithEndings, as_24_bit_terminal_escaped};

    struct Ctx {
        syntaxes: SyntaxSet,
        theme: Theme,
    }

    fn ctx() -> &'static Ctx {
        static CTX: OnceLock<Ctx> = OnceLock::new();
        CTX.get_or_init(|| Ctx {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: ThemeSet::load_defaults()
                .themes
                .remove("base16-ocean.dark")
                .expect("syntect ships base16-ocean.dark"),
        })
    }

    /// Render PKGBUILD source. Always ends with a single `\n` so the prompt
    /// that follows lands on a fresh line; passes `false` to the terminal
    /// escaper so the theme's background never paints over the user's bg.
    pub(super) fn pkgbuild(text: &str) -> String {
        render(text, ui::color_on())
    }

    fn render(text: &str, colors: bool) -> String {
        if !colors {
            return plain(text);
        }
        try_color(text).unwrap_or_else(|| plain(text))
    }

    fn plain(text: &str) -> String {
        if text.is_empty() || text.ends_with('\n') {
            return text.to_owned();
        }
        let mut s = String::with_capacity(text.len() + 1);
        s.push_str(text);
        s.push('\n');
        s
    }

    fn try_color(text: &str) -> Option<String> {
        if text.is_empty() {
            return Some(String::new());
        }
        let Ctx { syntaxes, theme } = ctx();
        let syntax = syntaxes
            .find_syntax_by_name("Bourne Again Shell (bash)")
            .or_else(|| syntaxes.find_syntax_by_extension("sh"))?;
        let mut hl = HighlightLines::new(syntax, theme);
        let mut out = String::with_capacity(text.len() * 2);
        for line in LinesWithEndings::from(text) {
            let ranges = hl.highlight_line(line, syntaxes).ok()?;
            out.push_str(&as_24_bit_terminal_escaped(&ranges, false));
        }
        // Move any trailing newline past the reset so the styled block ends
        // with `\x1b[0m\n` regardless of whether the source had a final \n.
        if out.ends_with('\n') {
            out.pop();
        }
        out.push_str("\u{1b}[0m\n");
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::render;
        use console::strip_ansi_codes;

        #[test]
        fn colored_roundtrips_to_source() {
            let src = "pkgname=foo\npkgver=1.2.3\n\nbuild() {\n    cd \"$srcdir/$pkgname-$pkgver\"  # comment\n    make\n}\n";
            let out = render(src, true);
            assert!(out.contains("\u{1b}["), "expected ANSI escapes: {out:?}");
            assert!(
                out.ends_with("\u{1b}[0m\n"),
                "missing final reset+nl: {out:?}"
            );
            // Strip the trailing reset before comparing, since strip_ansi_codes
            // leaves the surrounding text alone.
            assert_eq!(
                strip_ansi_codes(&out).trim_end_matches('\n'),
                src.trim_end_matches('\n')
            );
        }

        #[test]
        fn plain_when_colors_off() {
            let src = "pkgname=foo\n";
            assert_eq!(render(src, false), src);
        }

        #[test]
        fn adds_trailing_newline_when_source_lacks_one() {
            assert_eq!(render("pkgname=foo", false), "pkgname=foo\n");
            let out = render("pkgname=foo", true);
            assert!(out.ends_with("\u{1b}[0m\n"));
        }

        #[test]
        fn empty_input_stays_empty() {
            assert_eq!(render("", false), "");
            assert_eq!(render("", true), "");
        }

        #[test]
        fn utf8_in_pkgdesc_does_not_panic() {
            let src = "pkgdesc=\"héllo wörld — 漢字\"\n";
            let out = render(src, true);
            assert_eq!(
                strip_ansi_codes(&out).trim_end_matches('\n'),
                src.trim_end_matches('\n')
            );
        }
    }
}
