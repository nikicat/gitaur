//! Command vocabulary for the interactive shell + the line parser.
//!
//! Words-only (no pacman-letter clusters, no clap): a line is tokenized with
//! `shell_words` and the first token is matched against a verb. In phase 1 the
//! argument-bearing verbs keep their args as raw strings; later phases parse
//! them into `Selector`s (numbers / ranges / names / globs).

use crate::mirror::RefreshScope;
use crate::names::SearchTerm;

/// The canonical command verbs.
///
/// The typed identity of each command word: [`ALL`](Self::ALL) is the single
/// source of truth for the verbs `help` lists and the completer offers as
/// first-word / `help <topic>` candidates, and the per-verb side tables
/// (`help` topics, completion arg scopes) key off the enum so the compiler —
/// not a drift test — walks a new verb through every decision. Aliases
/// (`install`, `discard`, `up`, …) intentionally stay out — they live in
/// [`ALIASES`], and completion teaches the canonical name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Search,
    Info,
    Add,
    Drop,
    Keep,
    Remove,
    Upgrade,
    Review,
    Approve,
    Show,
    Apply,
    Undo,
    Redo,
    Clear,
    Refresh,
    System,
    Help,
    Quit,
}

impl Verb {
    /// Every verb, in help-list order.
    pub const ALL: &'static [Self] = &[
        Self::Search,
        Self::Info,
        Self::Add,
        Self::Drop,
        Self::Keep,
        Self::Remove,
        Self::Upgrade,
        Self::Review,
        Self::Approve,
        Self::Show,
        Self::Apply,
        Self::Undo,
        Self::Redo,
        Self::Clear,
        Self::Refresh,
        Self::System,
        Self::Help,
        Self::Quit,
    ];

    /// The word the user types (and `help` lists).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Info => "info",
            Self::Add => "add",
            Self::Drop => "drop",
            Self::Keep => "keep",
            Self::Remove => "remove",
            Self::Upgrade => "upgrade",
            Self::Review => "review",
            Self::Approve => "approve",
            Self::Show => "show",
            Self::Apply => "apply",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::Clear => "clear",
            Self::Refresh => "refresh",
            Self::System => "system",
            Self::Help => "help",
            Self::Quit => "quit",
        }
    }
}

/// Alias word → canonical verb — the one site for the alternate spellings.
///
/// [`parse`] resolves the first word through this after the canonical names,
/// and [`unknown_note`]'s typo suggester offers these words alongside them.
/// Completion deliberately stays canonical-only (see [`Verb`]).
pub const ALIASES: &[(&str, Verb)] = &[
    ("install", Verb::Add),
    ("discard", Verb::Drop),
    ("unstage", Verb::Drop),
    ("only", Verb::Keep),
    ("uninstall", Verb::Remove),
    ("rm", Verb::Remove),
    ("up", Verb::Upgrade),
    ("status", Verb::Show),
    ("ls", Verb::Show),
    ("cart", Verb::Show),
    ("commit", Verb::Apply),
    ("do", Verb::Apply),
    ("?", Verb::Help),
    ("exit", Verb::Quit),
    ("q", Verb::Quit),
];

/// Match one (already-lowercased) first word against the canonical names,
/// then the aliases.
fn verb_for(word: &str) -> Option<Verb> {
    Verb::ALL
        .iter()
        .copied()
        .find(|v| v.name() == word)
        .or_else(|| {
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == word)
                .map(|(_, v)| *v)
        })
}

/// `system <action>` — the maintenance sub-verbs.
///
/// A deliberate two-word group: `prune` deletes multi-GiB caches, so it hides
/// behind the `system` prefix where a mistyped single word can't reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAction {
    /// `system show` — disk usage of every state category.
    Show,
    /// `system prune` — delete the re-derivable caches.
    Prune,
}

impl SystemAction {
    /// Both actions, in help order — drives completion after `system`.
    pub const ALL: &'static [Self] = &[Self::Show, Self::Prune];

    /// The word the user types.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Show => "show",
            Self::Prune => "prune",
        }
    }

    /// Match one (already-lowercased) word against the actions.
    fn parse(word: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|a| a.name() == word)
    }
}

/// The typeable `refresh` scope words, in help order.
///
/// One site for the vocabulary, shared by [`parse`] and the completer so
/// they can't drift. Bare `refresh` means [`RefreshScope::Everything`],
/// which deliberately has no word: you get it by typing nothing.
pub const REFRESH_SCOPES: &[(RefreshScope, &str)] =
    &[(RefreshScope::Aur, "aur"), (RefreshScope::Pacman, "pacman")];

/// Resolve `refresh`'s optional argument: no word is the full refresh, a
/// scope word narrows it, and anything else is `None` (dispatch prints the
/// usage line — a typo'd scope must not silently widen to a full refresh).
fn parse_refresh_scope(arg: Option<&String>) -> Option<RefreshScope> {
    let Some(word) = arg else {
        return Some(RefreshScope::Everything);
    };
    let word = word.to_ascii_lowercase();
    REFRESH_SCOPES
        .iter()
        .find(|(_, w)| *w == word)
        .map(|(scope, _)| *scope)
}

/// One parsed shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `search <terms…>` — find packages across repos + AUR.
    Search(Vec<SearchTerm>),
    /// `info <pkg…>` — show package details.
    Info(Vec<String>),
    /// `add <pkg…>` — stage packages to install.
    Add(Vec<String>),
    /// `drop <pkg…>` — unstage packages from the cart.
    Drop(Vec<String>),
    /// `keep <pkg…>` — unstage everything *except* these — the inverse of `drop`.
    Keep(Vec<String>),
    /// `remove <pkg…>` — stage packages to uninstall.
    Remove(Vec<String>),
    /// `upgrade [pkg…]` — stage available upgrades (all, or the matching subset).
    Upgrade(Vec<String>),
    /// `review [pkg…]` — view a PKGBUILD/diff and approve it. No args reviews
    /// every AUR item in the cart still awaiting review.
    Review(Vec<String>),
    /// `approve <pkg…>` — approve staged AUR packages without opening a diff.
    Approve(Vec<String>),
    /// `show` — preview the staged transaction.
    Show,
    /// `apply` — build + install the staged transaction.
    Apply,
    /// `undo` — revert the last cart-changing command.
    Undo,
    /// `redo` — reapply the last undone change.
    Redo,
    /// `clear` — empty the cart.
    Clear,
    /// `refresh [aur|pacman]` — re-fetch the AUR mirror + index and/or the
    /// official sync DBs. `None` when the argument is unrecognized (dispatch
    /// prints the usage line, like `system`).
    Refresh(Option<RefreshScope>),
    /// `system <show|prune>` — state disk usage / cache pruning. `None` when
    /// the action is missing or unrecognized (dispatch prints the usage line).
    System(Option<SystemAction>),
    /// `help [command]` — command list (optional per-command topic).
    Help(Option<String>),
    /// `quit` / `exit` / Ctrl-D (or Ctrl-C at the prompt) — leave the shell.
    Quit,
    /// Blank or whitespace-only line — a no-op.
    Empty,
    /// First token didn't match any known verb; carries the verb as typed.
    Unknown(String),
    /// The line couldn't be tokenized (e.g. an unbalanced quote); carries the
    /// tokenizer's message.
    Syntax(String),
}

impl Command {
    /// The canonical [`Verb`] this command is an instance of — `None` for the
    /// non-verb lines (empty, unknown word, tokenizer error).
    pub const fn verb(&self) -> Option<Verb> {
        match self {
            Self::Search(_) => Some(Verb::Search),
            Self::Info(_) => Some(Verb::Info),
            Self::Add(_) => Some(Verb::Add),
            Self::Drop(_) => Some(Verb::Drop),
            Self::Keep(_) => Some(Verb::Keep),
            Self::Remove(_) => Some(Verb::Remove),
            Self::Upgrade(_) => Some(Verb::Upgrade),
            Self::Review(_) => Some(Verb::Review),
            Self::Approve(_) => Some(Verb::Approve),
            Self::Show => Some(Verb::Show),
            Self::Apply => Some(Verb::Apply),
            Self::Undo => Some(Verb::Undo),
            Self::Redo => Some(Verb::Redo),
            Self::Clear => Some(Verb::Clear),
            Self::Refresh(_) => Some(Verb::Refresh),
            Self::System(_) => Some(Verb::System),
            Self::Help(_) => Some(Verb::Help),
            Self::Quit => Some(Verb::Quit),
            Self::Empty | Self::Unknown(_) | Self::Syntax(_) => None,
        }
    }
}

/// Parse one input line into a [`Command`].
///
/// Never fails: tokenizer errors become [`Command::Syntax`] and an unrecognized
/// verb becomes [`Command::Unknown`], so a bad line reports and the REPL keeps
/// going rather than aborting the session.
pub fn parse(line: &str) -> Command {
    let tokens = match shell_words::split(line) {
        Ok(t) => t,
        Err(e) => return Command::Syntax(e.to_string()),
    };
    let Some((verb, args)) = tokens.split_first() else {
        return Command::Empty;
    };
    let args = args.to_vec();
    let Some(v) = verb_for(&verb.to_ascii_lowercase()) else {
        return Command::Unknown(verb.clone());
    };
    match v {
        Verb::Search => Command::Search(args.into_iter().map(SearchTerm::from).collect()),
        Verb::Info => Command::Info(args),
        Verb::Add => Command::Add(args),
        Verb::Drop => Command::Drop(args),
        Verb::Keep => Command::Keep(args),
        Verb::Remove => Command::Remove(args),
        Verb::Upgrade => Command::Upgrade(args),
        Verb::Review => Command::Review(args),
        Verb::Approve => Command::Approve(args),
        Verb::Show => Command::Show,
        Verb::Apply => Command::Apply,
        Verb::Undo => Command::Undo,
        Verb::Redo => Command::Redo,
        Verb::Clear => Command::Clear,
        Verb::Refresh => Command::Refresh(parse_refresh_scope(args.first())),
        Verb::System => Command::System(
            args.first()
                .and_then(|a| SystemAction::parse(&a.to_ascii_lowercase())),
        ),
        Verb::Help => Command::Help(args.into_iter().next()),
        Verb::Quit => Command::Quit,
    }
}

/// The message for an unrecognized first word.
///
/// A near-miss of a verb or alias gets "did you mean", an all-digit word gets
/// the row-selection hint (numbers are selectors, not commands), and anything
/// else is offered as a search — the launch shortcut (`aurox <term>`) already
/// gives bare terms that meaning.
pub fn unknown_note(word: &str) -> String {
    if word.bytes().all(|b| b.is_ascii_digit()) {
        return format!(
            "unknown command `{word}` — numbers select rows for a verb, e.g. `add {word}` or `info {word}`"
        );
    }
    let lower = word.to_ascii_lowercase();
    // Fuzzy-match only words long enough to carry a typo signal (a 1-2 letter
    // word is never "almost" a verb), and keep the allowed distance tight for
    // short words so `foo` doesn't get a far-fetched suggestion.
    let max_distance = match lower.len() {
        0..=2 => 0,
        3..=4 => 1,
        _ => 2,
    };
    let candidates = Verb::ALL
        .iter()
        .map(|v| v.name())
        .chain(ALIASES.iter().map(|(alias, _)| *alias));
    let best = candidates
        .map(|c| (levenshtein(&lower, c), c))
        .filter(|(d, _)| (1..=max_distance).contains(d))
        .min_by_key(|(d, _)| *d);
    match best {
        Some((_, near)) => {
            format!("unknown command `{word}` — did you mean `{near}`? (`help` lists commands)")
        }
        None => format!("unknown command `{word}` — try `search {word}`; `help` lists commands"),
    }
}

/// Plain byte-wise Levenshtein distance — the words compared are ASCII verbs
/// and user-typed first words, so per-byte editing is the right granularity.
fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != cb);
            current.push(substitution.min(prev[j + 1] + 1).min(current[j] + 1));
        }
        prev = current;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn terms(parts: &[&str]) -> Vec<SearchTerm> {
        parts.iter().map(|s| SearchTerm::from(*s)).collect()
    }

    #[test]
    fn parses_verb_and_args() {
        assert_eq!(
            parse("search foo bar"),
            Command::Search(terms(&["foo", "bar"]))
        );
    }

    #[test]
    fn empty_and_whitespace_are_empty() {
        assert_eq!(parse(""), Command::Empty);
        assert_eq!(parse("   \t "), Command::Empty);
    }

    #[test]
    fn verb_is_case_insensitive() {
        assert_eq!(parse("SEARCH x"), Command::Search(terms(&["x"])));
        assert_eq!(parse("Quit"), Command::Quit);
    }

    #[test]
    fn aliases_map_to_canonical() {
        assert_eq!(parse("install x"), Command::Add(v(&["x"])));
        assert_eq!(parse("discard x"), Command::Drop(v(&["x"])));
        assert_eq!(parse("only x"), Command::Keep(v(&["x"])));
        assert_eq!(parse("up"), Command::Upgrade(v(&[])));
        assert_eq!(parse("commit"), Command::Apply);
        assert_eq!(parse("do"), Command::Apply);
        assert_eq!(parse("status"), Command::Show);
        assert_eq!(parse("ls"), Command::Show);
        assert_eq!(parse("cart"), Command::Show);
        assert_eq!(parse("exit"), Command::Quit);
        assert_eq!(parse("q"), Command::Quit);
    }

    #[test]
    fn approve_takes_selectors_including_star() {
        assert_eq!(parse("approve yay-bin"), Command::Approve(v(&["yay-bin"])));
        assert_eq!(parse("approve *"), Command::Approve(v(&["*"])));
    }

    #[test]
    fn quoting_groups_tokens() {
        assert_eq!(
            parse(r#"add "name with space" other"#),
            Command::Add(v(&["name with space", "other"]))
        );
    }

    #[test]
    fn unterminated_quote_is_syntax_error() {
        assert!(matches!(parse("add \"x"), Command::Syntax(_)));
    }

    #[test]
    fn unknown_verb_carries_token() {
        assert_eq!(parse("frobnicate x"), Command::Unknown("frobnicate".into()));
    }

    #[test]
    fn help_takes_optional_topic() {
        assert_eq!(parse("help"), Command::Help(None));
        assert_eq!(parse("help add"), Command::Help(Some("add".into())));
    }

    #[test]
    fn every_alias_parses_to_its_verb() {
        // Structural since `parse` resolves through the table, but pins the
        // table's words as typeable (e.g. `?` survives tokenization).
        for (alias, verb) in ALIASES {
            assert_eq!(parse(alias).verb(), Some(*verb), "alias `{alias}`");
        }
    }

    #[test]
    fn unknown_note_suggests_a_near_miss_verb_or_alias() {
        assert_eq!(
            unknown_note("aprove"),
            "unknown command `aprove` — did you mean `approve`? (`help` lists commands)"
        );
        assert_eq!(
            unknown_note("serach"),
            "unknown command `serach` — did you mean `search`? (`help` lists commands)"
        );
        // Aliases are suggestion candidates too.
        assert_eq!(
            unknown_note("instal"),
            "unknown command `instal` — did you mean `install`? (`help` lists commands)"
        );
    }

    #[test]
    fn unknown_note_offers_search_when_nothing_is_close() {
        assert_eq!(
            unknown_note("3dslicer"),
            "unknown command `3dslicer` — try `search 3dslicer`; `help` lists commands"
        );
        // Short words get no far-fetched suggestion (`foo` is not "almost"
        // any verb worth proposing).
        assert_eq!(
            unknown_note("foo"),
            "unknown command `foo` — try `search foo`; `help` lists commands"
        );
    }

    #[test]
    fn unknown_note_teaches_that_numbers_are_selectors() {
        assert_eq!(
            unknown_note("3"),
            "unknown command `3` — numbers select rows for a verb, e.g. `add 3` or `info 3`"
        );
    }

    #[test]
    fn levenshtein_distances() {
        assert_eq!(levenshtein("approve", "approve"), 0);
        assert_eq!(levenshtein("aprove", "approve"), 1);
        assert_eq!(levenshtein("serach", "search"), 2);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn every_verb_round_trips_through_the_parser() {
        // Every advertised verb must parse to itself — guards `Verb::name`
        // against drifting from the `parse` match (a verb listed but
        // unhandled, or a renamed canonical name).
        for verb in Verb::ALL {
            let name = verb.name();
            assert_eq!(
                parse(name).verb(),
                Some(*verb),
                "`{name}` doesn't round-trip"
            );
        }
    }

    #[test]
    fn system_actions_round_trip_and_are_case_insensitive() {
        for action in SystemAction::ALL {
            let line = format!("system {}", action.name());
            assert_eq!(parse(&line), Command::System(Some(*action)), "`{line}`");
        }
        assert_eq!(
            parse("SYSTEM Prune"),
            Command::System(Some(SystemAction::Prune))
        );
    }

    #[test]
    fn system_without_or_with_unknown_action_parses_to_none() {
        // Dispatch turns the `None` into a usage line; the important part is
        // that neither form is `Unknown` (the verb itself was recognized) and
        // a typo'd action can never reach `prune`.
        assert_eq!(parse("system"), Command::System(None));
        assert_eq!(parse("system wat"), Command::System(None));
    }

    #[test]
    fn arg_only_verbs_ignore_extra_tokens() {
        // `show`/`apply`/`clear` take no args in phase 1.
        assert_eq!(parse("show now please"), Command::Show);
        assert_eq!(parse("apply"), Command::Apply);
    }

    #[test]
    fn refresh_scopes_parse_and_are_case_insensitive() {
        assert_eq!(
            parse("refresh"),
            Command::Refresh(Some(RefreshScope::Everything))
        );
        for (scope, word) in REFRESH_SCOPES {
            let line = format!("refresh {word}");
            assert_eq!(parse(&line), Command::Refresh(Some(*scope)), "`{line}`");
        }
        assert_eq!(
            parse("REFRESH Pacman"),
            Command::Refresh(Some(RefreshScope::Pacman))
        );
    }

    #[test]
    fn refresh_with_unknown_scope_parses_to_none() {
        // Dispatch turns the `None` into a usage line; a typo'd scope must
        // never silently widen into a full refresh.
        assert_eq!(parse("refresh everything-please"), Command::Refresh(None));
        assert_eq!(parse("refresh repos"), Command::Refresh(None));
    }
}
