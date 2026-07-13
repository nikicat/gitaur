//! Command vocabulary for the interactive shell + the line parser.
//!
//! Words-only (no pacman-letter clusters, no clap): a line is tokenized with
//! `shell_words` and the first token is matched against a verb. In phase 1 the
//! argument-bearing verbs keep their args as raw strings; later phases parse
//! them into `Selector`s (numbers / ranges / names / globs).

use crate::names::SearchTerm;

/// The canonical command verbs.
///
/// The typed identity of each command word: [`ALL`](Self::ALL) is the single
/// source of truth for the verbs `help` lists and the completer offers as
/// first-word / `help <topic>` candidates, and the per-verb side tables
/// (`help` topics, completion arg scopes) key off the enum so the compiler —
/// not a drift test — walks a new verb through every decision. Aliases
/// (`install`, `discard`, `up`, …) intentionally stay out — they live only in
/// [`parse`], and completion teaches the canonical name.
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
    /// `refresh` — re-fetch the AUR mirror + index.
    Refresh,
    /// `system <show|prune>` — state disk usage / cache pruning. `None` when
    /// the action is missing or unrecognized (dispatch prints the usage line).
    System(Option<SystemAction>),
    /// `help [command]` — command list (optional per-command topic).
    Help(Option<String>),
    /// `quit` / `exit` / Ctrl-D — leave the shell.
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
            Self::Refresh => Some(Verb::Refresh),
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
    match verb.to_ascii_lowercase().as_str() {
        "search" => Command::Search(args.into_iter().map(SearchTerm::from).collect()),
        "info" => Command::Info(args),
        "add" | "install" => Command::Add(args),
        "drop" | "discard" | "unstage" => Command::Drop(args),
        "keep" | "only" => Command::Keep(args),
        "remove" | "uninstall" | "rm" => Command::Remove(args),
        "upgrade" | "up" => Command::Upgrade(args),
        "review" => Command::Review(args),
        "approve" => Command::Approve(args),
        "show" | "status" | "ls" => Command::Show,
        "apply" | "commit" | "do" => Command::Apply,
        "undo" => Command::Undo,
        "redo" => Command::Redo,
        "clear" => Command::Clear,
        "refresh" => Command::Refresh,
        "system" => Command::System(
            args.first()
                .and_then(|a| SystemAction::parse(&a.to_ascii_lowercase())),
        ),
        "help" | "?" => Command::Help(args.into_iter().next()),
        "quit" | "exit" | "q" => Command::Quit,
        _ => Command::Unknown(verb.clone()),
    }
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
        // `show`/`apply`/`clear`/`refresh` take no args in phase 1.
        assert_eq!(parse("show now please"), Command::Show);
        assert_eq!(parse("apply"), Command::Apply);
    }
}
