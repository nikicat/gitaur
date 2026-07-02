//! Tab-completion for the interactive shell.
//!
//! A rustyline [`Helper`] whose [`Completer`] is **context-aware** and
//! **positional** (see the "Tab completion" section of `docs/plans/shell-ui.md`):
//!
//! | Cursor position | Completes to |
//! | --- | --- |
//! | the first word | command verbs |
//! | arg of `search` / `add` / `info` / `remove` | the full name universe |
//! | arg of `drop` / `keep` / `review` / `approve` / `upgrade` | names currently in the cart |
//! | arg of `help` | command verbs |
//! | a numeric token, or an arg of `show`/`apply`/… | nothing |
//!
//! The active verb is recovered by parsing the line *before* the word under the
//! cursor with the real [`command::parse`] — so aliases (`install`, `up`, …)
//! resolve to their canonical verb without a second alias table. The name
//! universe is shared with the session by `Rc` (rebuilt on `upgrade`/`refresh`);
//! the cart snapshot is refreshed after every command. Both come from the same
//! sources the [`selector`](super::selector) resolver uses, so "what Tab offers"
//! and "what the verb accepts" can't drift.

use super::command;
use crate::names::PkgTarget;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::borrow::Cow;
use std::rc::Rc;

/// Cap on candidates one Tab offers, so a bare `add <Tab>` over the ~100k-name
/// universe doesn't dump the whole index into the terminal. The universe is
/// sorted, so this shows the alphabetically-first matches; narrowing the prefix
/// reaches the rest.
const MAX_CANDIDATES: usize = 200;

/// Which set an argument position completes against, keyed off the canonical
/// verb. Mirrors the verb-scoping in [`State::dispatch`](super::State::dispatch):
/// list/universe verbs resolve against the whole name universe; cart verbs act
/// on what's staged.
enum ArgKind {
    /// `help <topic>` — completes verbs, like the first word.
    Verbs,
    /// `search`/`add`/`info`/`remove` — the full name universe.
    Universe,
    /// `drop`/`keep`/`review`/`approve`/`upgrade` — names currently in the cart.
    Cart,
    /// `show`/`apply`/`clear`/`refresh`/`quit` — nothing to complete.
    None,
}

/// What a canonical verb's arguments complete against.
const fn arg_kind(verb: &str) -> ArgKind {
    match verb.as_bytes() {
        b"help" => ArgKind::Verbs,
        b"search" | b"add" | b"info" | b"remove" => ArgKind::Universe,
        b"drop" | b"keep" | b"review" | b"approve" | b"upgrade" => ArgKind::Cart,
        _ => ArgKind::None,
    }
}

/// The shell's rustyline helper.
///
/// Carries the completion sources plus a history [`Hinter`] (the dimmed inline
/// suggestion of the last matching command); highlighting is a no-op except for
/// dimming that hint, and validation stays at the trait default (always-valid).
pub struct ShellHelper {
    /// The sorted, de-duplicated name universe (AUR pkgnames + pkgbases + sync
    /// names), shared with the session by `Rc` and replaced wholesale on
    /// `upgrade`/`refresh`. Sorted, so prefix completion is a binary search.
    universe: Rc<[PkgTarget]>,
    /// The specs currently staged in the cart, refreshed after each command —
    /// `PkgTarget`, like the universe, since these are names the user types to
    /// address a cart row (`drop yay-bin`), the same currency the
    /// [`selector`](super::selector) resolver accepts.
    cart: Vec<PkgTarget>,
    /// Suggests the tail of the most recent history entry that starts with the
    /// current line — rustyline's stock [`HistoryHinter`], reading the same
    /// history ring the editor persists. Right-arrow / End accepts the hint.
    hinter: HistoryHinter,
}

impl ShellHelper {
    /// A helper over `universe` with an empty cart (the session starts with
    /// nothing staged).
    pub fn new(universe: Rc<[PkgTarget]>) -> Self {
        Self {
            universe,
            cart: Vec::new(),
            hinter: HistoryHinter::new(),
        }
    }

    /// Refresh the per-command snapshots: the staged cart specs, and (cheaply,
    /// by `Rc`) the current name universe. Called after every dispatch so Tab
    /// reflects the just-mutated cart and any `upgrade`/`refresh` reload.
    pub fn sync(&mut self, universe: Rc<[PkgTarget]>, cart: Vec<PkgTarget>) {
        self.universe = universe;
        self.cart = cart;
    }

    /// The completion core, split out so it's unit-testable without a rustyline
    /// [`Context`] (which needs a live history). `pos` is the cursor's **byte
    /// offset** into `line`; the returned `usize` is the **byte offset where the
    /// replacement starts** (the start of the word under the cursor) — rustyline
    /// then swaps `line[start..pos]` for the chosen candidate. Both are
    /// rustyline's text-editing contract, not package identifiers.
    fn candidates(&self, line: &str, pos: usize) -> (usize, Vec<Pair>) {
        let start = word_start(line, pos);
        let word = &line[start..pos];
        let before = &line[..start];
        // First word (nothing but whitespace before the cursor word) → verbs.
        let cands = if before.trim().is_empty() {
            verb_candidates(word)
        } else {
            match arg_kind(command::parse(before).verb()) {
                ArgKind::Verbs => verb_candidates(word),
                ArgKind::Universe => self.name_candidates(word),
                ArgKind::Cart => prefix_pairs(self.cart.iter().map(PkgTarget::as_str), word),
                ArgKind::None => Vec::new(),
            }
        };
        (start, cands)
    }

    /// Universe names with the given prefix, via a binary search over the sorted
    /// slice, capped at [`MAX_CANDIDATES`].
    fn name_candidates(&self, prefix: &str) -> Vec<Pair> {
        if is_numeric_token(prefix) {
            return Vec::new();
        }
        let from = self.universe.partition_point(|t| t.as_str() < prefix);
        self.universe[from..]
            .iter()
            .map(PkgTarget::as_str)
            .take_while(|name| name.starts_with(prefix))
            .take(MAX_CANDIDATES)
            .map(name_pair)
            .collect()
    }
}

/// Canonical verbs with the given prefix. The replacement carries a trailing
/// space so a completed verb (`app` → `apply `) leaves the cursor ready for an
/// argument.
fn verb_candidates(prefix: &str) -> Vec<Pair> {
    command::VERBS
        .iter()
        .filter(|v| v.starts_with(prefix))
        .map(|v| Pair {
            display: (*v).to_owned(),
            replacement: format!("{v} "),
        })
        .collect()
}

/// Linear prefix filter over a small name set (the cart), as plain name pairs.
fn prefix_pairs<'a>(names: impl Iterator<Item = &'a str>, prefix: &str) -> Vec<Pair> {
    if is_numeric_token(prefix) {
        return Vec::new();
    }
    names
        .filter(|n| n.starts_with(prefix))
        .map(name_pair)
        .collect()
}

/// A package-name candidate: inserted verbatim, no trailing space (the user may
/// still append a glob suffix or keep typing).
fn name_pair(name: &str) -> Pair {
    Pair {
        display: name.to_owned(),
        replacement: name.to_owned(),
    }
}

/// Whether a token is the start of a numeric selector (`3`, `5-8`) — those index
/// the visible list and are never completed.
fn is_numeric_token(tok: &str) -> bool {
    tok.bytes().next().is_some_and(|b| b.is_ascii_digit())
}

/// Byte index where the word under the cursor begins: just past the last
/// whitespace before `pos`, or `0` if there is none. `pos` is a rustyline byte
/// offset on a char boundary.
fn word_start(line: &str, pos: usize) -> usize {
    line[..pos]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8())
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        Ok(self.candidates(line, pos))
    }
}

// Hints come from history (delegated to `HistoryHinter`); highlighting only
// dims that hint so it reads as a suggestion, not typed text; validation stays
// always-valid. `Helper` is the marker tying them together. rustyline only calls
// `highlight_hint` when its own colour mode is on, so `--color never` (mapped to
// `ColorMode::Disabled` in `shell::run`) renders the hint plain automatically.
impl Hinter for ShellHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        self.hinter.hint(line, pos, ctx)
    }
}
impl Highlighter for ShellHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        // Force styling: rustyline has already decided colour is on by calling
        // us, so emit the dim escape regardless of console's own tty sniffing.
        Cow::Owned(
            console::Style::new()
                .dim()
                .force_styling(true)
                .apply_to(hint)
                .to_string(),
        )
    }
}
impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper(universe: &[&str], cart: &[&str]) -> ShellHelper {
        let mut names: Vec<PkgTarget> = universe.iter().map(|n| PkgTarget::new(*n)).collect();
        names.sort_unstable();
        let mut h = ShellHelper::new(Rc::from(names.into_boxed_slice()));
        h.cart = cart.iter().map(|s| PkgTarget::new(*s)).collect();
        h
    }

    /// Complete at end-of-line and return the replacements, in offered order.
    fn complete(h: &ShellHelper, line: &str) -> Vec<String> {
        let (_, pairs) = h.candidates(line, line.len());
        pairs.into_iter().map(|p| p.replacement).collect()
    }

    #[test]
    fn first_word_completes_verbs_with_trailing_space() {
        let h = helper(&[], &[]);
        // `sea` is unique to `search`; the trailing space readies the cursor
        // for an argument.
        assert_eq!(complete(&h, "sea"), vec!["search "]);
        // `ap` is shared, so both survivors keep the trailing space.
        assert_eq!(complete(&h, "ap"), vec!["approve ", "apply "]);
    }

    #[test]
    fn first_word_prefix_narrows_verbs() {
        let h = helper(&[], &[]);
        let got = complete(&h, "re");
        assert_eq!(got, vec!["remove ", "review ", "refresh "]);
    }

    #[test]
    fn empty_first_word_offers_every_verb() {
        let h = helper(&[], &[]);
        assert_eq!(complete(&h, "").len(), command::VERBS.len());
    }

    #[test]
    fn add_arg_completes_the_universe_by_prefix() {
        let h = helper(&["python-foo", "python-bar", "ruby", "perl"], &[]);
        let got = complete(&h, "add py");
        // Universe order is sorted; both python-* match, nothing else.
        assert_eq!(got, vec!["python-bar", "python-foo"]);
    }

    #[test]
    fn universe_completion_works_through_aliases() {
        // `install` is an alias for `add`; its args still complete the universe.
        let h = helper(&["zlib", "zstd"], &[]);
        assert_eq!(complete(&h, "install zs"), vec!["zstd"]);
    }

    #[test]
    fn cart_verbs_complete_against_the_cart_not_the_universe() {
        let h = helper(&["firefox", "firefox-bin"], &["yay-bin", "cuda"]);
        // `drop` sees the cart…
        assert_eq!(complete(&h, "drop ya"), vec!["yay-bin"]);
        // …not the universe (which has `firefox*`, absent from the cart).
        assert!(complete(&h, "drop fire").is_empty());
    }

    #[test]
    fn approve_and_review_and_upgrade_use_the_cart() {
        let h = helper(&["other"], &["yay-bin"]);
        for line in ["approve ya", "review ya", "upgrade ya", "up ya", "keep ya"] {
            assert_eq!(complete(&h, line), vec!["yay-bin"], "line `{line}`");
        }
    }

    #[test]
    fn help_arg_completes_verbs() {
        let h = helper(&["addilade"], &[]); // a universe name starting "add"
        // `help ad` offers the *verb* `add`, not the universe name.
        assert_eq!(complete(&h, "help ad"), vec!["add "]);
    }

    #[test]
    fn numeric_arg_is_not_completed() {
        let h = helper(&["3dfx"], &["3-foo"]);
        assert!(complete(&h, "add 3").is_empty());
        assert!(complete(&h, "drop 3").is_empty());
    }

    #[test]
    fn no_arg_verbs_complete_nothing() {
        let h = helper(&["showcase"], &[]);
        assert!(complete(&h, "show s").is_empty());
        assert!(complete(&h, "apply x").is_empty());
    }

    #[test]
    fn second_arg_still_completes_in_the_same_scope() {
        let h = helper(&["alpha", "alto", "beta"], &[]);
        // After one completed arg, the next still draws from the universe.
        assert_eq!(complete(&h, "add beta al"), vec!["alpha", "alto"]);
    }

    #[test]
    fn replacement_start_is_the_word_under_the_cursor() {
        let h = helper(&["zlib"], &[]);
        let (start, _) = h.candidates("add zl", "add zl".len());
        assert_eq!(
            start, 4,
            "replacement begins at the partial name, not the verb"
        );
    }

    #[test]
    fn candidates_are_capped() {
        let many: Vec<String> = (0..MAX_CANDIDATES + 50)
            .map(|i| format!("pkg{i:04}"))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let h = helper(&refs, &[]);
        assert_eq!(complete(&h, "add pkg").len(), MAX_CANDIDATES);
    }
}
