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

use super::command::{self, SystemAction, Verb};
use crate::names::PkgTarget;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
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
    /// `system <action>` — the maintenance sub-verbs.
    SystemActions,
    /// `search`/`add`/`info`/`remove` — the full name universe.
    Universe,
    /// `drop`/`keep`/`review`/`approve`/`upgrade` — names currently in the cart.
    Cart,
    /// `show`/`apply`/`clear`/`refresh`/`quit` — nothing to complete.
    None,
}

/// What a canonical verb's arguments complete against. Exhaustive over
/// [`Verb`] so a new verb can't reach the completer undecided; `None` (a
/// non-verb line) completes nothing.
const fn arg_kind(verb: Option<Verb>) -> ArgKind {
    match verb {
        Some(Verb::Help) => ArgKind::Verbs,
        Some(Verb::System) => ArgKind::SystemActions,
        Some(Verb::Search | Verb::Add | Verb::Info | Verb::Remove) => ArgKind::Universe,
        Some(Verb::Drop | Verb::Keep | Verb::Review | Verb::Approve | Verb::Upgrade) => {
            ArgKind::Cart
        }
        Some(
            Verb::Show
            | Verb::Apply
            | Verb::Undo
            | Verb::Redo
            | Verb::Clear
            | Verb::Refresh
            | Verb::Quit,
        )
        | None => ArgKind::None,
    }
}

/// The canonical verb names, in help order — the command-position word list.
fn verb_names() -> impl Iterator<Item = &'static str> {
    Verb::ALL.iter().map(|v| v.name())
}

/// The `system` action names, in help order.
fn action_names() -> impl Iterator<Item = &'static str> {
    SystemAction::ALL.iter().map(|a| a.name())
}

/// The shell's rustyline helper.
///
/// Carries the completion sources, which also drive the dimmed inline **hint**
/// (type-ahead) — see [`ShellHelper::hint_for`]. Highlighting is a no-op except
/// for dimming that hint, and validation stays at the trait default
/// (always-valid).
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
}

impl ShellHelper {
    /// A helper over `universe` with an empty cart (the session starts with
    /// nothing staged).
    pub const fn new(universe: Rc<[PkgTarget]>) -> Self {
        Self {
            universe,
            cart: Vec::new(),
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
            word_candidates(verb_names(), word)
        } else {
            match arg_kind(command::parse(before).verb()) {
                ArgKind::Verbs => word_candidates(verb_names(), word),
                ArgKind::SystemActions => word_candidates(action_names(), word),
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

    /// The dimmed inline type-ahead hint for `line` with the cursor at `pos`:
    /// the certain tail of what the user is typing under the cursor.
    ///
    /// Two policies, keyed on position (the [`arg_kind`] scoping the Tab
    /// completer already uses):
    /// - **command position** (word 1, or a `help <topic>` argument) — always
    ///   type-ahead the *first* matching verb, even when several match (`re` →
    ///   `move`, offering `remove`).
    /// - **package position** (`add`/`drop`/… arguments) — hint the **longest
    ///   common prefix** of all matching names beyond what's typed: the part
    ///   every candidate agrees on, so it's always correct even when the full
    ///   name is still ambiguous. A single match extends to the whole name
    ///   (`firefo` → `x`); two names sharing a stem extend to it (`asdzxc` +
    ///   `asdqwe`, `a` → `sd`); and once the next character diverges (`asd`,
    ///   `add a` thousands deep) there's nothing certain to add, so no hint.
    ///
    /// Only fires at end-of-line and on a non-empty word (there's no prefix to
    /// extend otherwise). Shares the Tab completer's sources, so the hint can
    /// never suggest something Tab wouldn't complete. Split out from the
    /// [`Hinter`] impl so it's unit-testable without a live history [`Context`].
    fn hint_for(&self, line: &str, pos: usize) -> Option<String> {
        // Type-ahead only extends the tail: skip unless the cursor is at the end
        // of a non-empty word.
        if pos != line.len() {
            return None;
        }
        let start = word_start(line, pos);
        let word = &line[start..pos];
        if word.is_empty() {
            return None;
        }
        let before = &line[..start];
        if before.trim().is_empty() {
            return word_hint(verb_names(), word);
        }
        match arg_kind(command::parse(before).verb()) {
            ArgKind::Verbs => word_hint(verb_names(), word),
            ArgKind::SystemActions => word_hint(action_names(), word),
            ArgKind::Universe => self.universe_hint(word),
            ArgKind::Cart => cart_hint(&self.cart, word),
            ArgKind::None => None,
        }
    }

    /// Package hint over the sorted name universe: the longest-common-prefix
    /// extension of every name starting with `word`. The matching names are a
    /// contiguous, sorted run, so their common prefix is just the common prefix
    /// of the run's first and last entries — found with two binary searches, no
    /// scan of the (possibly huge) matching range.
    fn universe_hint(&self, word: &str) -> Option<String> {
        if is_numeric_token(word) {
            return None;
        }
        let from = self.universe.partition_point(|t| t.as_str() < word);
        let tail = &self.universe[from..];
        let n = tail.partition_point(|t| t.as_str().starts_with(word));
        let matches = tail.get(..n)?;
        let (first, last) = (matches.first()?.as_str(), matches.last()?.as_str());
        common_prefix_suffix(first, last, word)
    }
}

/// Command-position hint: the tail of the *first* word (verb or `system`
/// action) starting with `word`, plus a trailing space (readying the cursor
/// for an argument, like Tab). `None` when nothing matches or `word` is
/// already whole (only the space would be left — not worth a hint).
fn word_hint(mut words: impl Iterator<Item = &'static str>, word: &str) -> Option<String> {
    let hit = words.find(|w| w.starts_with(word))?;
    let suffix = &hit[word.len()..];
    (!suffix.is_empty()).then(|| format!("{suffix} "))
}

/// Package hint over the (small, unsorted) cart: the longest-common-prefix
/// extension of every staged name starting with `word`. Folded against the
/// first match, since the cart isn't sorted for a first/last shortcut. `None`
/// when the common prefix is just `word` (nothing certain to add).
fn cart_hint(cart: &[PkgTarget], word: &str) -> Option<String> {
    if is_numeric_token(word) {
        return None;
    }
    let mut matches = cart
        .iter()
        .map(PkgTarget::as_str)
        .filter(|n| n.starts_with(word));
    let first = matches.next()?;
    // Every match starts with `word`, so the common-prefix length is ≥ word.len().
    let lcp = matches.fold(first.len(), |acc, n| acc.min(common_prefix_len(first, n)));
    let suffix = &first[word.len()..lcp];
    (!suffix.is_empty()).then(|| suffix.to_owned())
}

/// The part of the longest common prefix of `first`..`last` that extends beyond
/// `word` — the certain type-ahead tail. `first`/`last` are the lexicographic
/// bounds of a set all starting with `word`, so their common prefix is the whole
/// set's. `None` when the common prefix is just `word` (the next char diverges,
/// nothing certain to add).
fn common_prefix_suffix(first: &str, last: &str, word: &str) -> Option<String> {
    let lcp = &first[..common_prefix_len(first, last)];
    let suffix = lcp.strip_prefix(word)?;
    (!suffix.is_empty()).then(|| suffix.to_owned())
}

/// Byte length of the longest common prefix of `a` and `b`, always on a char
/// boundary.
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.char_indices()
        .zip(b.chars())
        .take_while(|((_, ca), cb)| ca == cb)
        .map(|((i, ca), _)| i + ca.len_utf8())
        .last()
        .unwrap_or(0)
}

/// Command-position words (verbs or `system` actions) with the given prefix.
/// The replacement carries a trailing space so a completed word (`app` →
/// `apply `) leaves the cursor ready for an argument.
fn word_candidates(words: impl Iterator<Item = &'static str>, prefix: &str) -> Vec<Pair> {
    words
        .filter(|w| w.starts_with(prefix))
        .map(|w| Pair {
            display: w.to_owned(),
            replacement: format!("{w} "),
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

// The hint is completion-driven type-ahead (see `hint_for`), not history;
// highlighting only dims it so it reads as a suggestion, not typed text;
// validation stays always-valid. `Helper` is the marker tying them together.
// rustyline only calls `highlight_hint` when its own colour mode is on, so
// `--color never` (mapped to `ColorMode::Disabled` in `shell::run`) renders the
// hint plain automatically.
impl Hinter for ShellHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        self.hint_for(line, pos)
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
        assert_eq!(got, vec!["remove ", "review ", "redo ", "refresh "]);
    }

    #[test]
    fn empty_first_word_offers_every_verb() {
        let h = helper(&[], &[]);
        assert_eq!(complete(&h, "").len(), Verb::ALL.len());
    }

    #[test]
    fn system_arg_completes_the_actions() {
        let h = helper(&["showcase", "pruneyard"], &[]);
        // The `system` argument position offers the sub-verbs — never the
        // universe names that share the prefix.
        assert_eq!(complete(&h, "system "), vec!["show ", "prune "]);
        assert_eq!(complete(&h, "system pr"), vec!["prune "]);
        assert_eq!(complete(&h, "system sh"), vec!["show "]);
    }

    #[test]
    fn system_arg_hints_the_first_matching_action() {
        let h = helper(&[], &[]);
        assert_eq!(hint(&h, "system pr").as_deref(), Some("une "));
        assert_eq!(hint(&h, "system s").as_deref(), Some("how "));
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

    /// The type-ahead hint for a line, cursor at end.
    fn hint(h: &ShellHelper, line: &str) -> Option<String> {
        h.hint_for(line, line.len())
    }

    #[test]
    fn verb_hint_always_takes_the_first_match_even_when_ambiguous() {
        let h = helper(&[], &[]);
        // `re` matches remove/review/refresh; the command position always hints
        // the first (VERBS order → `remove`). Trailing space rides along so
        // accepting readies the cursor for an argument.
        assert_eq!(hint(&h, "re").as_deref(), Some("move "));
        // `app` matches both approve and apply; VERBS order puts approve first.
        assert_eq!(hint(&h, "app").as_deref(), Some("rove "));
        // A prefix unique to one verb hints the rest of it.
        assert_eq!(hint(&h, "appl").as_deref(), Some("y "));
    }

    #[test]
    fn a_fully_typed_verb_hints_nothing() {
        let h = helper(&[], &[]);
        // The only remaining "completion" is the trailing space — not worth a hint.
        assert_eq!(hint(&h, "apply"), None);
    }

    #[test]
    fn package_hint_extends_to_the_longest_common_prefix() {
        let h = helper(&["firefox", "firefox-bin", "zlib"], &[]);
        // firefox / firefox-bin agree up to `firefox` → hint the shared stem,
        // even though the full name is still ambiguous.
        assert_eq!(hint(&h, "add fire").as_deref(), Some("fox"));
        // At the divergence point (`firefox` then end vs `-`) nothing is certain.
        assert_eq!(hint(&h, "add firefox"), None);
        // A single match extends to the whole name.
        assert_eq!(hint(&h, "add zl").as_deref(), Some("ib"));
        assert_eq!(hint(&h, "add firefox-").as_deref(), Some("bin"));
    }

    #[test]
    fn package_hint_stops_where_candidates_diverge() {
        // The motivating case: only `asdzxc` and `asdqwe` exist. `a` is certain
        // up to `asd` (their common stem), then diverges.
        let h = helper(&["asdzxc", "asdqwe", "unrelated"], &[]);
        assert_eq!(hint(&h, "add a").as_deref(), Some("sd"));
        assert_eq!(hint(&h, "add asd"), None); // z vs q — ambiguous next char
        assert_eq!(hint(&h, "add asdz").as_deref(), Some("xc")); // now unique
    }

    #[test]
    fn cart_hint_extends_to_the_common_prefix_too() {
        let h = helper(&["other"], &["yay-git", "yay-bin", "zsh"]);
        // Both cart items share `yay-` → hint the stem.
        assert_eq!(hint(&h, "drop ya").as_deref(), Some("y-"));
        // Divergence (`git` vs `bin`) → nothing certain.
        assert_eq!(hint(&h, "drop yay-"), None);
        // Single match → whole tail.
        assert_eq!(hint(&h, "drop zs").as_deref(), Some("h"));
    }

    #[test]
    fn help_topic_hint_always_takes_the_first_verb() {
        // `help <topic>` is a command position, so it type-aheads like word 1.
        let h = helper(&["remotething"], &[]);
        assert_eq!(hint(&h, "help re").as_deref(), Some("move "));
    }

    #[test]
    fn no_hint_mid_word_or_on_empty_or_no_arg_verb() {
        let h = helper(&["zlib"], &[]);
        // Cursor not at end of line → no type-ahead.
        assert_eq!(h.hint_for("add zlib", 3), None);
        // Empty word (trailing space) → nothing to extend.
        assert_eq!(hint(&h, "add "), None);
        assert_eq!(hint(&h, ""), None);
        // A no-argument verb takes no package hint.
        assert_eq!(hint(&h, "show z"), None);
        // Numeric selectors are never hinted.
        assert_eq!(hint(&h, "add 3"), None);
    }
}
