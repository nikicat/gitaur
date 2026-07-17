//! Shared test scaffolding for the shell's dispatch tests: the scripted
//! [`ShellEnv`] fake plus the little builders every submodule's tests use.
//!
//! `#[cfg(test)]`-only (declared as such in `shell.rs`); items are
//! `pub(super)` so each sibling's `tests` mod can drive [`State::dispatch`]
//! against [`FakeEnv`] without re-implementing the 15-method trait.

use super::cart::{ApplyOutcome, AurApproval, Cart, ReviewOutcome, Source, StageClass};
use super::command;
use super::{Flow, ListItem, ListSource, NumberedList, ShellEnv, State};
use crate::error::Result;
use crate::index;
use crate::mirror;
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName, SearchTerm};
use crate::pacman::invoke::PkgUpgrade;
use crate::system;
use crate::units::ByteSize;
use std::collections::HashMap;

/// The fake env's captured output: every `print`ed line, in order.
///
/// A named domain type rather than a bare `Vec<String>` — but deliberately
/// *not* [`ui::Table`], which is specifically *rendered-table* lines built
/// only inside `ui` (and whose own doc warns against conflating it with other
/// string lists). This is a transcript of arbitrary shell output, exposing
/// the substring assertions the tests actually make rather than a raw `Vec`.
#[derive(Default, Debug)]
pub(super) struct Transcript(Vec<String>);

impl Transcript {
    pub(super) fn push(&mut self, line: &str) {
        self.0.push(line.to_owned());
    }
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Some printed line contains `needle` — the common assertion.
    pub(super) fn contains(&self, needle: &str) -> bool {
        self.0.iter().any(|l| l.contains(needle))
    }
    /// Some printed line satisfies `pred`, for compound / exact-match checks.
    pub(super) fn any(&self, pred: impl Fn(&str) -> bool) -> bool {
        self.0.iter().any(|l| pred(l))
    }
    /// The whole transcript as one string, for cross-line substring checks.
    pub(super) fn joined(&self) -> String {
        self.0.join("\n")
    }
    /// How many printed lines contain `needle` — for once-per-batch checks.
    pub(super) fn count_containing(&self, needle: &str) -> usize {
        self.0.iter().filter(|l| l.contains(needle)).count()
    }
}

/// How many times a scripted env effect ran. A typed counter so the fake's
/// call bookkeeping reads `env.upgrades.count()` against a named type instead
/// of a bare `usize` that could be compared against anything.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallCount(usize);

impl CallCount {
    pub(super) fn bump(&mut self) {
        self.0 += 1;
    }
    pub(super) fn count(self) -> usize {
        self.0
    }
}

/// Scripted [`ShellEnv`] capturing output + recording calls, with a
/// pre-seeded search result, name universe, classification table, and
/// scripted review/apply outcomes, so dispatch is testable without a
/// terminal, index, or alpm.
#[derive(Default)]
pub(super) struct FakeEnv {
    pub(super) lines: Transcript,
    pub(super) upgrades: CallCount,
    pub(super) refreshes: CallCount,
    pub(super) search_result: Vec<ListItem>,
    pub(super) info_calls: Vec<Vec<PkgTarget>>,
    pub(super) names: Vec<PkgTarget>,
    /// What `upgrade` returns (the recomputed candidates to seed).
    pub(super) upgrade_candidates: Vec<PkgUpgrade>,
    /// spec → coarse source; absent ⇒ `classify` returns `None`.
    pub(super) classes: HashMap<String, Source>,
    pub(super) policy: AurApproval,
    /// spec → review verdict; absent ⇒ `Approved`.
    pub(super) review_outcomes: HashMap<String, ReviewOutcome>,
    pub(super) review_calls: Vec<String>,
    /// What `apply` returns; absent ⇒ `Succeeded`.
    pub(super) apply_outcome: Option<ApplyOutcome>,
    pub(super) apply_calls: CallCount,
    /// Rows `system_usage` reports (under a fixed `/state` root).
    pub(super) usage_rows: Vec<system::Usage>,
    /// Scripted `system_prune` outcome: `Some(freed)` = confirmed,
    /// `None` = the user declined the prompt.
    pub(super) prune_outcome: Option<ByteSize>,
    pub(super) prune_calls: CallCount,
    /// What `refresh` reports; `None` ⇒ a full `Refreshed`.
    pub(super) refresh_outcome: Option<mirror::RefreshOutcome>,
    /// The scopes `refresh` was called with, in order.
    pub(super) refresh_scopes: Vec<mirror::RefreshScope>,
    /// What `aur_state` reports; `None` ⇒ `Ready` (no nudges).
    pub(super) aur_state: Option<index::AurState>,
    /// How often the transaction table rendered — the quiet-mutation rule's
    /// observable: `show` renders, `add`/`drop`/… must not.
    pub(super) render_calls: CallCount,
}

impl ShellEnv for FakeEnv {
    fn print(&mut self, line: &str) {
        self.lines.push(line);
    }
    fn upgrade(&mut self) -> Result<Vec<PkgUpgrade>> {
        self.upgrades.bump();
        Ok(self.upgrade_candidates.clone())
    }
    fn refresh(&mut self, scope: mirror::RefreshScope) -> Result<mirror::RefreshOutcome> {
        self.refreshes.bump();
        self.refresh_scopes.push(scope);
        Ok(self
            .refresh_outcome
            .unwrap_or(mirror::RefreshOutcome::Refreshed))
    }
    fn search(&mut self, _terms: &[SearchTerm]) -> Result<Vec<ListItem>> {
        // The numbered table print is RealEnv's side of the seam (like
        // `render_cart`); the fake only supplies the selector rows.
        Ok(self.search_result.clone())
    }
    fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()> {
        self.info_calls.push(targets.to_vec());
        Ok(())
    }
    fn names(&self) -> &[PkgTarget] {
        &self.names
    }
    fn classify(&self, target: &PkgTarget) -> Option<StageClass> {
        // The fake tracks only the coarse source; the concrete repo (which
        // drives the display + `drop core`) is exercised via `upgrade`-seeded
        // rows, whose `from_upgrade` carries the real repo name.
        self.classes
            .get(target.as_str())
            .map(|&source| StageClass { source, repo: None })
    }
    fn aur_policy(&self) -> AurApproval {
        self.policy
    }
    fn aur_state(&self) -> index::AurState {
        // Most dispatch tests don't care; `Ready` keeps them nudge-free.
        self.aur_state.unwrap_or(index::AurState::Ready)
    }
    fn pkgbase_of(&self, target: &PkgTarget) -> Option<PkgBase> {
        Some(PkgBase::from(target.as_str()))
    }
    fn review(&mut self, target: &PkgTarget) -> Result<ReviewOutcome> {
        self.review_calls.push(target.as_str().to_owned());
        Ok(self
            .review_outcomes
            .remove(target.as_str())
            .unwrap_or(ReviewOutcome::Approved))
    }
    fn render_cart(&mut self, _cart: &Cart) {
        // The table rendering (color, alignment, age) is RealEnv's job; the
        // pure dispatch core under test prints the header + summary itself.
        // The call count is the tests' proof of which verbs draw the table.
        self.render_calls.bump();
    }
    fn apply(&mut self, _cart: &Cart) -> Result<ApplyOutcome> {
        self.apply_calls.bump();
        Ok(self.apply_outcome.take().unwrap_or(ApplyOutcome::Succeeded))
    }
    fn system_usage(&mut self) -> system::Report {
        system::Report {
            root: std::path::PathBuf::from("/state"),
            rows: self.usage_rows.clone(),
        }
    }
    fn system_prune(&mut self) -> Result<Option<ByteSize>> {
        self.prune_calls.bump();
        Ok(self.prune_outcome)
    }
}

/// A `FakeEnv` that classifies the given specs (everything else is unknown).
pub(super) fn env_with(classes: &[(&str, Source)]) -> FakeEnv {
    let mut env = FakeEnv::default();
    for (spec, source) in classes {
        env.classes.insert((*spec).to_owned(), *source);
    }
    env
}

pub(super) fn li(name: &str) -> ListItem {
    ListItem {
        target: PkgTarget::new(name),
        repo: None,
    }
}

/// A list row tagged with its repo, for the `add <repo>` filter tests.
pub(super) fn li_repo(repo: &str, name: &str) -> ListItem {
    ListItem {
        target: PkgTarget::new(name),
        repo: Some(RepoName::from(repo)),
    }
}

/// A `State` with a numbered search table "on screen": its referent is a
/// search-list snapshot over `rows`, as if that table had just printed.
pub(super) fn state_showing(rows: Vec<ListItem>) -> State {
    State {
        referent: Some(NumberedList {
            source: ListSource::Search,
            rows,
        }),
        ..State::default()
    }
}

pub(super) fn dispatch_one(input: &str) -> (Flow, FakeEnv) {
    let mut env = FakeEnv::default();
    let mut state = State::default();
    let flow = state.dispatch(&command::parse(input), &mut env);
    (flow, env)
}

pub(super) fn up(repo: &str, name: &str) -> PkgUpgrade {
    use crate::version::Version;
    PkgUpgrade {
        repo: RepoName::from(repo),
        name: PkgName::from(name),
        old_ver: Version::from("1-1"),
        new_ver: Version::from("2-1"),
    }
}

/// The staged install specs, in cart order — the assertion view of the cart
/// the repo-filter tests compare against.
pub(super) fn cart_specs(state: &State) -> Vec<PkgTarget> {
    state
        .cart
        .items()
        .iter()
        .map(|i| PkgTarget::new(i.spec()))
        .collect()
}
