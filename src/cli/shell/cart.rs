//! The staged transaction the shell builds up, run by `apply`.
//!
//! `add`/`drop`/`remove`/`clear` mutate a [`Cart`]; `upgrade` seeds it with the
//! available upgrades; `review`/`approve` move AUR items past the approval gate;
//! `apply` runs the whole thing in one go. None of it is persisted — quitting
//! drops the cart. See `docs/plans/shell-ui.md` for the design.
//!
//! This module is the pure data model: staging, dedup, and the approval-state
//! transitions, all unit-tested here without I/O. The side effects the verbs
//! need (coarse repo/AUR classification, the PKGBUILD diff, the build+install)
//! live behind the [`super::ShellEnv`] trait.

use crate::build::Target;
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName};
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use std::collections::HashSet;

/// Where a staged install came from.
///
/// Decides auto-approval and how `show` labels the row. The *install routing*
/// (which `pacman` lane it takes) is re-decided by the resolver at apply time;
/// this tag only drives the approval policy and the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// In a sync repo — pacman owns its provenance, so it auto-approves.
    Repo,
    /// In the AUR index — has a PKGBUILD, so it needs review by default.
    Aur,
}

impl Source {
    /// Display label for the `show` table.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Aur => "aur",
        }
    }
}

/// Coarse staging classification of a name: the source lane plus, for repo
/// packages, the concrete sync-DB it lives in (`core`, `extra`, …).
///
/// The concrete `repo` is display-only — it drives the `show` table's repo
/// column and the `drop core`/`add extra` repo-filter selectors. The real
/// install routing is still the resolver's call at apply time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageClass {
    pub source: Source,
    /// Concrete sync-repo for [`Source::Repo`]; `None` for AUR.
    pub repo: Option<RepoName>,
}

/// How AUR packages enter the cart: needing review, or pre-approved.
///
/// Derived from config `review_default` (`"skip"` ⇒ [`Self::Auto`]). A named
/// type rather than a bare bool so a call site reads `AurApproval::Auto`, not
/// `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AurApproval {
    /// AUR items stage as [`Approval::NeedsReview`] (the default).
    #[default]
    Review,
    /// AUR items stage pre-approved (`review_default == "skip"`).
    Auto,
}

/// Whether a staged item still needs the user's eyes on its PKGBUILD before
/// `apply` will run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Cleared for `apply` (a repo package, or an AUR one the user approved).
    Approved,
    /// An AUR package the user hasn't reviewed/approved yet.
    NeedsReview,
}

impl Approval {
    /// The approval state a freshly-staged item gets, given its source and the
    /// AUR policy. Repo packages always auto-approve; AUR packages follow
    /// `aur`.
    pub const fn default_for(source: Source, aur: AurApproval) -> Self {
        match source {
            Source::Repo => Self::Approved,
            Source::Aur => match aur {
                AurApproval::Auto => Self::Approved,
                AurApproval::Review => Self::NeedsReview,
            },
        }
    }

    /// Display label for the `show` table.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::NeedsReview => "review",
        }
    }
}

/// One staged install/upgrade: the target plus the bookkeeping the cart tracks.
#[derive(Debug, Clone)]
pub struct CartItem {
    /// Carries the counterpart hint through expand → resolve → prepare, exactly
    /// like the upgrade loop. Fresh `add` items are unhinted; `upgrade` seeds
    /// hinted ones (the foreign pkgname).
    pub target: Target,
    pub source: Source,
    pub approval: Approval,
    /// Concrete sync-repo (`core`, `extra`, …) for a repo package; `None` for
    /// AUR rows and for repo rows staged before the source DB was known. Drives
    /// the `show` table's repo column and the `drop core` repo filter — see
    /// [`Self::repo_label`].
    pub repo: Option<RepoName>,
    /// `Some` when this row came from `upgrade` — it carries the old→new
    /// versions for the `show` table and routes repo rows through the partial
    /// `pacman -Syu` lane at apply (rather than a fresh `pacman -S`).
    pub upgrade: Option<PkgUpgrade>,
}

impl CartItem {
    /// Stage a fresh install of `target` from `source`, defaulting the approval
    /// per `source` + the AUR policy. `repo` is the concrete sync-DB for a repo
    /// package (display only). The build pipeline's [`Target`] starts unhinted —
    /// `resolver::expand_pkgbase_targets` infers a counterpart hint on rewrite
    /// if needed.
    pub fn new(
        target: PkgTarget,
        source: Source,
        repo: Option<RepoName>,
        aur: AurApproval,
    ) -> Self {
        Self {
            target: Target::bare(target.into_inner()),
            source,
            approval: Approval::default_for(source, aur),
            repo,
            upgrade: None,
        }
    }

    /// Stage an upgrade candidate (from `upgrade`). The source follows the
    /// candidate's repo (`REPO_AUR` ⇒ AUR), and AUR rows hint their foreign
    /// pkgname so the counterpart resolves to the right installed pkg — exactly
    /// what the loop's `resolve_aur` did.
    pub fn from_upgrade(u: PkgUpgrade, aur: AurApproval) -> Self {
        let source = if u.repo == REPO_AUR {
            Source::Aur
        } else {
            Source::Repo
        };
        let target = match source {
            Source::Aur => Target::with_hint(u.name.as_str().to_owned(), u.name.clone()),
            Source::Repo => Target::bare(u.name.as_str().to_owned()),
        };
        // AUR rows label as `aur` from the source; a repo row carries its
        // concrete sync-DB so the table shows `core`/`extra`/… not just `repo`.
        let repo = (source == Source::Repo).then(|| u.repo.clone());
        Self {
            target,
            source,
            approval: Approval::default_for(source, aur),
            repo,
            upgrade: Some(u),
        }
    }

    /// The freeform user-typed spec this item stages.
    pub fn spec(&self) -> &str {
        &self.target.spec
    }

    /// The repo bucket this row displays in and a repo filter matches against:
    /// the concrete sync-DB for a known repo package, `aur` for an AUR row, or
    /// `repo` when a repo package was staged before its source DB was resolved.
    pub fn repo_label(&self) -> RepoName {
        match (self.source, &self.repo) {
            (Source::Aur, _) => RepoName::from(REPO_AUR),
            (Source::Repo, Some(r)) => r.clone(),
            (Source::Repo, None) => RepoName::from("repo"),
        }
    }

    /// A repo *upgrade* row — applied via the partial `pacman -Syu` lane, not a
    /// fresh `pacman -S`.
    pub fn is_repo_upgrade(&self) -> bool {
        self.source == Source::Repo && self.upgrade.is_some()
    }

    /// `old → new` for an upgrade row, `None` for a fresh install (for `show`).
    pub fn version_transition(&self) -> Option<String> {
        self.upgrade
            .as_ref()
            .map(|u| format!("{} → {}", u.old_ver, u.new_ver))
    }
}

/// The outcome the dispatch core uses to update the cart after `env.apply`.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// User declined at the confirm gate — the cart is left untouched.
    Declined,
    /// Everything installed/removed cleanly — the applied rows leave the cart.
    Succeeded,
    /// The transaction ran but something failed or was interrupted. `installed`
    /// carries the staged install rows that *did* land, so the cart drops them
    /// and keeps only the offenders — the ones still to `drop`/fix and retry.
    /// Staged removals stay put (they don't run once a build fails). Empty when
    /// nothing landed at all.
    Failed { installed: Vec<PkgTarget> },
}

/// What staging an item (`add` / `stage_remove`) did to the cart — a named
/// outcome rather than a bare `bool` so the call site reads as intent and pairs
/// with [`ApproveResult`] / [`UnstageResult`].
#[derive(Debug, PartialEq, Eq)]
pub enum StageResult {
    /// The item was newly staged.
    Staged,
    /// The spec was already staged — re-staging is an idempotent no-op.
    AlreadyStaged,
}

/// What `drop` (`unstage`) did to the cart.
#[derive(Debug, PartialEq, Eq)]
pub enum UnstageResult {
    /// A staged row was removed.
    Unstaged,
    /// Nothing in the cart matched the target.
    NotStaged,
}

/// What `keep` did to the cart — the set-complement of [`UnstageResult`], where
/// `drop` names the rows to remove and `keep` names the rows to spare.
#[derive(Debug, PartialEq, Eq)]
pub enum KeepResult {
    /// No staged install matched the keep-set — the cart is left untouched, so a
    /// mistyped `keep` can't silently empty it.
    NoMatch,
    /// Kept the matched rows and dropped the rest; carries the dropped specs (in
    /// cart order) as [`PkgTarget`]s for the caller to report. Empty when the
    /// keep-set already covered every staged install (a no-op `keep`).
    Kept { dropped: Vec<PkgTarget> },
}

/// What `approve <spec>` did to a staged item — so the caller can report it and
/// know whether it newly cleared the gate (and should record the pkgbase as
/// reviewed).
#[derive(Debug, PartialEq, Eq)]
pub enum ApproveResult {
    /// The spec isn't in the cart.
    NotStaged,
    /// It was already approved — nothing changed.
    AlreadyApproved,
    /// It moved from `NeedsReview` to `Approved`.
    Approved,
}

/// What one `review` of an AUR pkgbase decided.
#[derive(Debug, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// User approved the PKGBUILD — clear the item for `apply`.
    Approved,
    /// User chose "approve all" — clear this item *and* every remaining one in
    /// the pass without opening another diff.
    ApprovedAll,
    /// User looked but deferred — the item stays `NeedsReview`.
    Skipped,
    /// User aborted the whole review pass — stop, leave the rest as they are.
    Aborted,
}

/// The pending transaction. Built across many commands, run by `apply`.
#[derive(Default)]
pub struct Cart {
    /// Staged installs/upgrades (repo + AUR), each with its approval state.
    items: Vec<CartItem>,
    /// Packages staged for uninstall → `pacman -R` at apply.
    remove: Vec<PkgName>,
    /// PKGBUILDs approved this session, keyed by pkgbase — threaded into the
    /// build pipeline so it doesn't re-prompt a diff the user already cleared
    /// in the shell (survives discard/re-add and post-failure retries).
    reviewed: HashSet<PkgBase>,
}

impl Cart {
    /// Nothing staged on either side.
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty() && self.remove.is_empty()
    }

    /// The staged install rows, in staging order.
    pub fn items(&self) -> &[CartItem] {
        &self.items
    }

    /// The staged removals, in staging order.
    pub fn removals(&self) -> &[PkgName] {
        &self.remove
    }

    /// Pkgbases already reviewed this session — fed to the build pipeline to
    /// suppress repeat diffs.
    pub const fn reviewed(&self) -> &HashSet<PkgBase> {
        &self.reviewed
    }

    /// Stage one install. Returns `false` (and stages nothing) when the spec is
    /// already in the cart — re-`add`ing is idempotent, not a duplicate row.
    ///
    /// Inserts keeping [`Self::items`] sorted (repo-rank → repo → name) so the
    /// row number `show` prints *is* the vector index `resolve_against_cart`
    /// addresses — the two can't drift (`docs/plans/shell-ui.md`, phase 5b).
    pub fn add(&mut self, item: CartItem) -> StageResult {
        if self.items.iter().any(|i| i.spec() == item.spec()) {
            return StageResult::AlreadyStaged;
        }
        self.items.push(item);
        self.sort_items();
        StageResult::Staged
    }

    /// Re-establish the cart's sort invariant: rows grouped by repo
    /// (repo-rank → concrete repo name), then by spec within a repo — the same
    /// order the unified `show` table renders. The cart is tiny, so a full
    /// re-sort per `add` is cheaper than threading a sorted-insert position.
    /// `unstage` / `approve` / `clear_applied` preserve relative order, so only
    /// the inserting paths need this.
    fn sort_items(&mut self) {
        self.items.sort_by(|a, b| {
            let (ra, rb) = (a.repo_label(), b.repo_label());
            ra.rank()
                .cmp(&rb.rank())
                .then_with(|| ra.as_str().cmp(rb.as_str()))
                .then_with(|| a.spec().cmp(b.spec()))
        });
    }

    /// Unstage an install. Reports whether a row was removed.
    pub fn unstage(&mut self, target: &PkgTarget) -> UnstageResult {
        let before = self.items.len();
        self.items.retain(|i| i.spec() != target.as_str());
        if self.items.len() == before {
            UnstageResult::NotStaged
        } else {
            UnstageResult::Unstaged
        }
    }

    /// Keep only the staged installs whose spec is in `keep`, dropping every
    /// other install row — the inverse of [`Self::unstage`]: `drop` names the
    /// rows to remove, `keep` names the rows to spare (handy for narrowing a
    /// large `upgrade`-seeded cart down to a few packages).
    ///
    /// Removals are left untouched — `keep` mirrors `drop`, which only unstages
    /// installs. Guards against emptying the cart on a typo: when no staged
    /// install matches, returns [`KeepResult::NoMatch`] and changes nothing.
    /// Relative order of the kept rows is preserved, so the sorted-cart
    /// invariant holds without a re-sort.
    pub fn keep(&mut self, keep: &HashSet<&str>) -> KeepResult {
        if !self.items.iter().any(|i| keep.contains(i.spec())) {
            return KeepResult::NoMatch;
        }
        let dropped = self
            .items
            .iter()
            .filter(|i| !keep.contains(i.spec()))
            .map(|i| PkgTarget::new(i.spec()))
            .collect();
        self.items.retain(|i| keep.contains(i.spec()));
        KeepResult::Kept { dropped }
    }

    /// Stage a removal (uninstall). [`StageResult::AlreadyStaged`] when it was
    /// already staged for removal.
    pub fn stage_remove(&mut self, name: PkgName) -> StageResult {
        if self.remove.contains(&name) {
            return StageResult::AlreadyStaged;
        }
        self.remove.push(name);
        StageResult::Staged
    }

    /// Empty everything — installs, removals, and the reviewed set.
    pub fn clear(&mut self) {
        self.items.clear();
        self.remove.clear();
        self.reviewed.clear();
    }

    /// Drop the installs + removals after a clean `apply`, but keep the
    /// reviewed set so a later re-`add` of the same pkgbase isn't re-prompted.
    pub fn clear_applied(&mut self) {
        self.items.clear();
        self.remove.clear();
    }

    /// The staged item matching `target`, if any.
    pub fn item(&self, target: &PkgTarget) -> Option<&CartItem> {
        self.items.iter().find(|i| i.spec() == target.as_str())
    }

    /// Record that `pkgbase`'s PKGBUILD was reviewed this session.
    pub fn mark_reviewed(&mut self, pkgbase: PkgBase) {
        self.reviewed.insert(pkgbase);
    }

    /// Approve the staged item for `target`, reporting what changed. The caller
    /// records the pkgbase as reviewed only on [`ApproveResult::Approved`].
    pub fn approve(&mut self, target: &PkgTarget) -> ApproveResult {
        match self.items.iter_mut().find(|i| i.spec() == target.as_str()) {
            None => ApproveResult::NotStaged,
            Some(i) if i.approval == Approval::Approved => ApproveResult::AlreadyApproved,
            Some(i) => {
                i.approval = Approval::Approved;
                ApproveResult::Approved
            }
        }
    }

    /// The AUR items still blocking `apply` — those that haven't been approved.
    pub fn pending_review(&self) -> Vec<&CartItem> {
        self.items
            .iter()
            .filter(|i| i.approval == Approval::NeedsReview)
            .collect()
    }

    /// Whether every staged item is cleared for `apply`.
    pub fn all_approved(&self) -> bool {
        self.items.iter().all(|i| i.approval == Approval::Approved)
    }

    /// The targets the install/build half of `apply` resolves through the `-S`
    /// pipeline: AUR rows (install or upgrade) and fresh repo installs. Repo
    /// *upgrades* are excluded — they go through the partial `pacman -Syu` lane
    /// ([`Self::repo_upgrades`]).
    pub fn install_targets(&self) -> Vec<Target> {
        self.items
            .iter()
            .filter(|i| !i.is_repo_upgrade())
            .map(|i| i.target.clone())
            .collect()
    }

    /// The staged repo *upgrade* rows, applied via `pacman -Syu` (ignoring every
    /// repo upgrade candidate the user didn't stage).
    pub fn repo_upgrades(&self) -> Vec<&PkgUpgrade> {
        self.items
            .iter()
            .filter(|i| i.is_repo_upgrade())
            .filter_map(|i| i.upgrade.as_ref())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(spec: &str, source: Source) -> CartItem {
        CartItem::new(PkgTarget::new(spec), source, None, AurApproval::Review)
    }

    fn target(spec: &str) -> PkgTarget {
        PkgTarget::new(spec)
    }

    #[test]
    fn repo_items_auto_approve_aur_items_need_review() {
        assert_eq!(item("glibc", Source::Repo).approval, Approval::Approved);
        assert_eq!(item("yay-bin", Source::Aur).approval, Approval::NeedsReview);
    }

    #[test]
    fn aur_auto_approve_policy_skips_review() {
        let it = CartItem::new(target("yay-bin"), Source::Aur, None, AurApproval::Auto);
        assert_eq!(it.approval, Approval::Approved);
    }

    #[test]
    fn add_dedups_by_spec() {
        let mut cart = Cart::default();
        assert_eq!(cart.add(item("foo", Source::Aur)), StageResult::Staged);
        assert_eq!(
            cart.add(item("foo", Source::Aur)),
            StageResult::AlreadyStaged,
            "re-add is a no-op"
        );
        assert_eq!(cart.items().len(), 1);
    }

    #[test]
    fn unstage_removes_by_target() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        assert_eq!(cart.unstage(&target("foo")), UnstageResult::Unstaged);
        assert_eq!(
            cart.unstage(&target("foo")),
            UnstageResult::NotStaged,
            "second drop finds nothing"
        );
        assert_eq!(cart.items().len(), 1);
        assert_eq!(cart.items()[0].spec(), "bar");
    }

    fn keep_set<'a>(specs: &'a [&str]) -> HashSet<&'a str> {
        specs.iter().copied().collect()
    }

    #[test]
    fn keep_drops_everything_but_the_selected() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        cart.add(item("baz", Source::Aur));
        // Keep only `bar` — the two AUR rows drop, reported in cart order.
        assert_eq!(
            cart.keep(&keep_set(&["bar"])),
            KeepResult::Kept {
                dropped: vec![PkgTarget::new("baz"), PkgTarget::new("foo")]
            }
        );
        let specs: Vec<&str> = cart.items().iter().map(CartItem::spec).collect();
        assert_eq!(specs, vec!["bar"]);
    }

    #[test]
    fn keep_matching_nothing_leaves_the_cart_intact() {
        // A keep-set that hits no staged row must not empty the cart (typo guard).
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        assert_eq!(cart.keep(&keep_set(&["absent"])), KeepResult::NoMatch);
        assert_eq!(cart.items().len(), 2, "nothing dropped on no match");
    }

    #[test]
    fn keep_covering_the_whole_cart_drops_nothing() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        // Every staged row is kept → a no-op, with an empty dropped list.
        assert_eq!(
            cart.keep(&keep_set(&["foo", "bar"])),
            KeepResult::Kept {
                dropped: Vec::new()
            }
        );
        assert_eq!(cart.items().len(), 2);
    }

    #[test]
    fn keep_leaves_removals_untouched() {
        // `keep` mirrors `drop` — it acts on installs only, not staged removals.
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.stage_remove(PkgName::from("old"));
        assert!(matches!(
            cart.keep(&keep_set(&["foo"])),
            KeepResult::Kept { .. }
        ));
        assert_eq!(cart.removals(), &[PkgName::from("old")]);
    }

    #[test]
    fn stage_remove_dedups() {
        let mut cart = Cart::default();
        assert_eq!(cart.stage_remove(PkgName::from("old")), StageResult::Staged);
        assert_eq!(
            cart.stage_remove(PkgName::from("old")),
            StageResult::AlreadyStaged
        );
        assert_eq!(cart.removals().len(), 1);
    }

    #[test]
    fn gate_blocks_until_aur_items_approved() {
        let mut cart = Cart::default();
        cart.add(item("glibc", Source::Repo));
        cart.add(item("yay-bin", Source::Aur));
        assert!(!cart.all_approved());
        assert_eq!(cart.pending_review().len(), 1);
        assert_eq!(cart.pending_review()[0].spec(), "yay-bin");

        cart.approve(&target("yay-bin"));
        assert!(cart.all_approved());
        assert!(cart.pending_review().is_empty());
    }

    #[test]
    fn approve_reports_the_transition() {
        let mut cart = Cart::default();
        cart.add(item("yay-bin", Source::Aur));
        assert_eq!(cart.approve(&target("yay-bin")), ApproveResult::Approved);
        assert_eq!(
            cart.approve(&target("yay-bin")),
            ApproveResult::AlreadyApproved
        );
        assert_eq!(cart.approve(&target("absent")), ApproveResult::NotStaged);
        assert!(cart.all_approved());
    }

    #[test]
    fn repo_only_cart_is_immediately_approved() {
        let mut cart = Cart::default();
        cart.add(item("glibc", Source::Repo));
        assert!(cart.all_approved());
    }

    #[test]
    fn clear_empties_everything_including_reviewed() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.stage_remove(PkgName::from("old"));
        cart.mark_reviewed(PkgBase::from("foo"));
        cart.clear();
        assert!(cart.is_empty());
        assert!(cart.reviewed().is_empty());
    }

    #[test]
    fn clear_applied_keeps_reviewed() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.stage_remove(PkgName::from("old"));
        cart.mark_reviewed(PkgBase::from("foo"));
        cart.clear_applied();
        assert!(cart.is_empty());
        assert!(
            cart.reviewed().contains(&PkgBase::from("foo")),
            "reviewed set survives a clean apply"
        );
    }

    #[test]
    fn install_targets_lists_every_staged_spec() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        let targets = cart.install_targets();
        let specs: Vec<&str> = targets.iter().map(|t| t.spec.as_str()).collect();
        // Sorted-cart invariant: `bar` (repo, ranks before AUR) precedes `foo`
        // (aur, sorts last) regardless of staging order.
        assert_eq!(specs, vec!["bar", "foo"]);
    }

    #[test]
    fn add_keeps_items_sorted_by_repo_then_name() {
        let mut cart = Cart::default();
        // Stage in deliberately scrambled order across repos.
        cart.add(CartItem::from_upgrade(
            upgrade("aur", "yay-bin"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            upgrade("extra", "vim"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            upgrade("core", "zlib"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            upgrade("core", "glibc"),
            AurApproval::Review,
        ));
        // core (alphabetical within repo) → extra → aur last.
        let order: Vec<&str> = cart.items().iter().map(CartItem::spec).collect();
        assert_eq!(order, vec!["glibc", "zlib", "vim", "yay-bin"]);
    }

    fn upgrade(repo: &str, name: &str) -> PkgUpgrade {
        use crate::version::Version;
        PkgUpgrade {
            repo: RepoName::from(repo),
            name: PkgName::from(name),
            old_ver: Version::from("1-1"),
            new_ver: Version::from("2-1"),
        }
    }

    #[test]
    fn from_upgrade_tags_source_and_hint() {
        let aur = CartItem::from_upgrade(upgrade("aur", "yay-bin"), AurApproval::Review);
        assert_eq!(aur.source, Source::Aur);
        assert_eq!(aur.approval, Approval::NeedsReview);
        assert_eq!(
            aur.target.hint.as_ref().map(PkgName::as_str),
            Some("yay-bin")
        );
        assert!(!aur.is_repo_upgrade());
        // AUR rows label `aur` from the source, not a stored concrete repo.
        assert_eq!(aur.repo, None);
        assert_eq!(aur.repo_label(), "aur");

        let repo = CartItem::from_upgrade(upgrade("core", "glibc"), AurApproval::Review);
        assert_eq!(repo.source, Source::Repo);
        assert_eq!(repo.approval, Approval::Approved);
        assert!(repo.is_repo_upgrade());
        assert_eq!(repo.version_transition().as_deref(), Some("1-1 → 2-1"));
        // A repo row carries its concrete sync-DB for the table's repo column.
        assert_eq!(repo.repo_label(), "core");
    }

    #[test]
    fn repo_label_falls_back_when_source_db_unknown() {
        // A repo package staged without a concrete repo (e.g. a fresh `add`
        // before classification surfaced the DB) still labels as `repo`.
        assert_eq!(item("glibc", Source::Repo).repo_label(), "repo");
        // AUR always labels `aur`.
        assert_eq!(item("yay-bin", Source::Aur).repo_label(), "aur");
    }

    #[test]
    fn repo_upgrades_split_from_install_targets() {
        let mut cart = Cart::default();
        cart.add(CartItem::from_upgrade(
            upgrade("core", "glibc"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            upgrade("aur", "yay-bin"),
            AurApproval::Review,
        ));
        cart.add(item("firefox", Source::Repo)); // a fresh repo install
        // Repo upgrades take the -Syu lane; the rest take the -S/build pipeline.
        assert_eq!(
            cart.repo_upgrades()
                .iter()
                .map(|u| u.name.as_str())
                .collect::<Vec<_>>(),
            vec!["glibc"]
        );
        // Sorted-cart invariant: firefox (repo, ranks before AUR) precedes
        // yay-bin (aur, sorts last).
        let install: Vec<String> = cart.install_targets().into_iter().map(|t| t.spec).collect();
        assert_eq!(install, vec!["firefox", "yay-bin"]);
    }
}
