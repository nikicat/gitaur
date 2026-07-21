//! The frozen whole-cart transaction, resolved once at `add`/`upgrade` time.
//!
//! The cart used to store a name + coarse tag and let `apply` re-resolve it —
//! so the provenance the user chose at `add` could be re-derived and disagree
//! (the `webp-pixbuf-loader` namesake bug). Now `add` resolves the *entire*
//! cart, freezes the plan into a [`ResolvedCart`], and stores it inside the
//! [`Cart`](super::cart::Cart); `show` renders it and `apply` executes it with
//! **no resolution**. Any `refresh` clears the cart, so the DBs can't move
//! between `add` and `apply` and the frozen plan stays valid.
//!
//! Pure data (no I/O), held behind an `Rc` so the undo snapshot
//! ([`State::edit_cart`](super::State)) captures the roots and the plan
//! together and the clone is cheap.

use super::upgrade::PreflightNote;
use crate::names::PkgName;
use crate::pacman::alpm_db::PacmanIndex;
use crate::resolver::Plan;
use crate::ui::UpgradeSelection;

/// The resolved transaction for the current cart: the plans `apply` executes
/// plus the display data `show` renders, all computed in one add-time pass.
///
/// Not `Clone`: it holds a [`PacmanIndex`] and [`PreviewMetrics`] (neither
/// cheap nor `Clone`), so it lives behind `Rc` and the cart clones the pointer.
pub(crate) struct ResolvedCart {
    /// AUR rebuilds whose install unblocks the repo `-Su` lane (sysupgrade
    /// blockers) — built and `pacman -U`'d before the repo upgrade. `None` when
    /// none are staged.
    pub blocker_plan: Option<Plan>,
    /// The install/build half: AUR rows (install or upgrade) and fresh repo
    /// installs. `None` for a repo-upgrade-only or removal-only cart.
    pub main_plan: Option<Plan>,
    /// The partial `pacman -Su` selection: the staged repo upgrades, with every
    /// other current repo candidate `--ignore`d. Frozen so `apply` upgrades
    /// exactly the versions the last refresh fetched.
    pub repo_sel: UpgradeSelection,
    /// The sysupgrade preflight verdict for the repo lane, frozen at add.
    pub preflight: FrozenPreflight,
    /// The rootless-synced pacman snapshot the sizes/versions were resolved
    /// against — carries the new repo versions for the `show` table and the
    /// cost summary. Frozen so `show`/`apply` don't reopen alpm. The lighter
    /// display rows (pulled-in deps, the build-time overlay) are re-derived from
    /// the frozen plans at render — cheap, and they never re-run the resolver.
    pub size_pac: PacmanIndex,
}

/// Whether the frozen repo-upgrade preflight needs the user's go-ahead at
/// `apply`. A named verdict rather than a bare `bool`, so the apply site reads
/// the *consequence*, not a flag.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreflightGate {
    /// No breakage pacman would hit remains — every flagged issue is either
    /// absent or covered by a staged rebuild ([`FrozenPreflight::blockers`]).
    /// `apply` runs the repo lane without asking.
    #[default]
    Clear,
    /// Breakage remains that a staged rebuild does *not* cover — `apply` prints
    /// the frozen notes and asks the override before running the repo lane
    /// (the synced snapshot is advisory, so walking away means no).
    NeedsOverride,
}

/// The sysupgrade preflight verdict for the staged repo lane, computed once at
/// add: the rendered notes plus the blocker/gate split `apply` consumes.
#[derive(Default)]
pub(crate) struct FrozenPreflight {
    /// The preflight notes — "upgrading X breaks Y" plus the shell-native way
    /// out — rendered under the `show` table and again ahead of `apply`.
    pub notes: Vec<PreflightNote>,
    /// Staged AUR rebuild targets that resolve a flagged breakage — `apply`
    /// installs them ahead of the repo lane instead of blocking on them.
    pub blockers: Vec<PkgName>,
    /// Whether unresolved breakage remains, gating the repo lane at `apply`.
    pub gate: PreflightGate,
}
