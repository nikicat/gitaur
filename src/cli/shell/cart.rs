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
use crate::names::{PkgBase, PkgName, PkgTarget};
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
    /// `Some` when this row came from `upgrade` — it carries the old→new
    /// versions for the `show` table and routes repo rows through the partial
    /// `pacman -Syu` lane at apply (rather than a fresh `pacman -S`).
    pub upgrade: Option<PkgUpgrade>,
}

impl CartItem {
    /// Stage a fresh install of `target` from `source`, defaulting the approval
    /// per `source` + the AUR policy. The build pipeline's [`Target`] starts
    /// unhinted — `resolver::expand_pkgbase_targets` infers a counterpart hint
    /// on rewrite if needed.
    pub fn new(target: PkgTarget, source: Source, aur: AurApproval) -> Self {
        Self {
            target: Target::bare(target.into_inner()),
            source,
            approval: Approval::default_for(source, aur),
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
        Self {
            target,
            source,
            approval: Approval::default_for(source, aur),
            upgrade: Some(u),
        }
    }

    /// The freeform user-typed spec this item stages.
    pub fn spec(&self) -> &str {
        &self.target.spec
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
    /// The transaction ran but something failed or was interrupted — the cart
    /// is kept intact so the user can `drop` the offender and `apply` the rest.
    Failed,
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
    pub fn add(&mut self, item: CartItem) -> bool {
        if self.items.iter().any(|i| i.spec() == item.spec()) {
            return false;
        }
        self.items.push(item);
        true
    }

    /// Unstage an install. Returns whether a row was removed.
    pub fn unstage(&mut self, target: &PkgTarget) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.spec() != target.as_str());
        self.items.len() != before
    }

    /// Stage a removal (uninstall). Returns `false` when already staged.
    pub fn stage_remove(&mut self, name: PkgName) -> bool {
        if self.remove.contains(&name) {
            return false;
        }
        self.remove.push(name);
        true
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
        CartItem::new(PkgTarget::new(spec), source, AurApproval::Review)
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
        let it = CartItem::new(target("yay-bin"), Source::Aur, AurApproval::Auto);
        assert_eq!(it.approval, Approval::Approved);
    }

    #[test]
    fn add_dedups_by_spec() {
        let mut cart = Cart::default();
        assert!(cart.add(item("foo", Source::Aur)));
        assert!(!cart.add(item("foo", Source::Aur)), "re-add is a no-op");
        assert_eq!(cart.items().len(), 1);
    }

    #[test]
    fn unstage_removes_by_target() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        assert!(cart.unstage(&target("foo")));
        assert!(!cart.unstage(&target("foo")), "second drop finds nothing");
        assert_eq!(cart.items().len(), 1);
        assert_eq!(cart.items()[0].spec(), "bar");
    }

    #[test]
    fn stage_remove_dedups() {
        let mut cart = Cart::default();
        assert!(cart.stage_remove(PkgName::from("old")));
        assert!(!cart.stage_remove(PkgName::from("old")));
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
        assert_eq!(specs, vec!["foo", "bar"]);
    }

    fn upgrade(repo: &str, name: &str) -> PkgUpgrade {
        use crate::version::Version;
        PkgUpgrade {
            repo: repo.to_owned(),
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

        let repo = CartItem::from_upgrade(upgrade("core", "glibc"), AurApproval::Review);
        assert_eq!(repo.source, Source::Repo);
        assert_eq!(repo.approval, Approval::Approved);
        assert!(repo.is_repo_upgrade());
        assert_eq!(repo.version_transition().as_deref(), Some("1-1 → 2-1"));
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
        let install: Vec<String> = cart.install_targets().into_iter().map(|t| t.spec).collect();
        assert_eq!(install, vec!["yay-bin", "firefox"]);
    }
}
