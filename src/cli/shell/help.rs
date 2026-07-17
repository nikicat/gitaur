//! The `help` command's text: the flat command list and the per-verb topics.

use super::command::{self, Verb};

/// The `help` command body. A flat command list; per-command topics land with
/// the commands themselves.
pub(super) const HELP_TEXT: &str = "\
commands:
  search <terms…>       find packages (repo + AUR)
  info <sel…>           show package details (sel = name | number | range | glob)
  add <sel…>            stage packages to install
  drop <sel…>           unstage packages from the cart (alias: discard)
  keep <sel…>           keep only these staged packages, drop the rest
  remove <sel…>         stage packages to uninstall
  upgrade [pkg…]        upgrade installed packages (repo + AUR)
  review [sel…]         view a PKGBUILD/diff and approve it (no sel = review all)
  approve <sel…>        approve staged AUR packages without a diff (try `approve *`)
  show                  preview the staged transaction
  apply                 build + install the staged transaction
  undo                  revert the last cart change
  redo                  reapply the last undone change
  clear                 empty the cart
  refresh [aur|pacman]  re-fetch the AUR mirror and/or the official repo DBs
  system show|prune     disk usage of aurox's state / delete the caches
  help [topic]          this list, or `help <command>` for detail on one
  quit                  leave the shell (also: Ctrl-D)
selectors: `3` (row), `5-8` (range), `glibc` (name), `python-*` (glob),
           `aur`/`core`/… (whole repo — e.g. `drop aur`, `add extra`)
a number names a row of the last numbered table printed — the search results
(`search`) or the transaction (`show`, and `upgrade`/`undo` print through it)";

/// Per-command help shown by `help <topic>`, keyed by canonical [`Verb`] (the
/// same order as [`Verb::ALL`]). Each body opens with a usage line (and any
/// aliases) then a short paragraph — enough to answer "what does this verb do
/// and what does it act on" without leaving the shell.
const TOPICS: &[(Verb, &str)] = &[
    (
        Verb::Search,
        "search <terms…>\n  \
         Query repos + AUR by name, description, and provides. Prints a numbered,\n  \
         ranked list (best matches nearest the prompt) and remembers it, so a later\n  \
         `add 3` / `info 1-4` can index it by number.",
    ),
    (
        Verb::Info,
        "info <sel…>\n  \
         Show package details. sel = name, number (a row in the shown list), range\n  \
         (`5-8`), or glob (`python-*`).",
    ),
    (
        Verb::Add,
        "add <sel…>   (alias: install)\n  \
         Stage packages to install in the pending transaction. Resolves against the\n  \
         last list, the AUR index, and the sync DBs — you can add anything.",
    ),
    (
        Verb::Drop,
        "drop <sel…>   (aliases: discard, unstage)\n  \
         Un-stage packages from the cart — resolves against what's staged. `drop aur`\n  \
         un-stages every AUR row. Distinct from `remove`, which stages an uninstall.",
    ),
    (
        Verb::Keep,
        "keep <sel…>   (alias: only)\n  \
         Keep only the selected staged packages and drop the rest — the inverse of\n  \
         `drop`.",
    ),
    (
        Verb::Remove,
        "remove <sel…>   (aliases: uninstall, rm)\n  \
         Stage an uninstall (`pacman -R`) in the transaction. Note the difference from\n  \
         `drop`: `drop` un-stages a pending install, `remove` stages a removal.",
    ),
    (
        Verb::Upgrade,
        "upgrade [sel…]   (alias: up)\n  \
         Refresh, recompute the available upgrades, and stage them (repo → approved,\n  \
         AUR → needs review). With sel…, stage only the matching subset.",
    ),
    (
        Verb::Review,
        "review [sel…]\n  \
         Open a PKGBUILD/diff for staged AUR packages and approve / skip / discard\n  \
         each. No sel reviews every AUR item still awaiting review.",
    ),
    (
        Verb::Approve,
        "approve <sel…>\n  \
         Approve staged AUR packages without opening a diff. `approve *` approves\n  \
         every staged AUR package at once.",
    ),
    (
        Verb::Show,
        "show   (aliases: status, ls)\n  \
         Preview the staged transaction: the change-set table with download sizes,\n  \
         build time, and totals. Its row numbers become what a bare number\n  \
         (`drop 2`) addresses, until the next numbered table prints.",
    ),
    (
        Verb::Apply,
        "apply   (aliases: commit, do)\n  \
         Build + install the staged transaction in one sudo batch. Runs only when\n  \
         every staged package is approved; an interrupted or failed apply drops back\n  \
         to the shell with the cart intact so you can `drop` the offender and retry.",
    ),
    (
        Verb::Undo,
        "undo\n  \
         Revert the last cart-changing command (add / drop / keep / remove /\n  \
         upgrade / approve / clear) — e.g. undo a `keep` that dropped too much.\n  \
         Steps back through the session's edits; `redo` reapplies. A run\n  \
         (`apply`) forgets the history.",
    ),
    (
        Verb::Redo,
        "redo\n  \
         Reapply the change `undo` just reverted. Available until the next\n  \
         cart-changing command, which forks a new edit branch.",
    ),
    (Verb::Clear, "clear\n  Empty the cart."),
    (
        Verb::Refresh,
        "refresh [aur|pacman]\n  \
         Re-fetch package data and reload the session — fresh data for\n  \
         search / info / upgrade / completion. Leaves the cart untouched.\n  \
         No argument refreshes everything: the AUR mirror + index and the\n  \
         official repo databases; `refresh aur` / `refresh pacman` narrow\n  \
         it to one half. If the AUR was never synced (you answered\n  \
         \"later\" at launch), a bare `refresh` stays pacman-only —\n  \
         `refresh aur` runs the one-time ~2 GiB clone; typing it is the\n  \
         consent, so there is no second question.",
    ),
    (
        Verb::System,
        "system <show|prune>\n  \
         Maintenance for aurox's on-disk state. `system show` prints each\n  \
         category's disk usage (AUR mirror, package index, repo db snapshot,\n  \
         build worktrees, logs, …). `system prune` deletes the re-derivable\n  \
         caches after a y/N confirm — the next `refresh aur` / build\n  \
         recreates them (pruning the AUR mirror means the next AUR sync\n  \
         runs the full ~2 GiB re-clone); build metrics and shell history\n  \
         are never touched.",
    ),
    (
        Verb::Help,
        "help [topic]\n  \
         List the commands, or `help <command>` for detail on one.",
    ),
    (
        Verb::Quit,
        "quit   (aliases: exit, q; also Ctrl-D)\n  Leave the shell.",
    ),
];

/// Detailed help for one `help <topic>` argument. Canonicalizes `topic` through
/// [`command::parse`] so aliases (`discard`, `up`, `ls`, …) resolve to their verb
/// for free, then looks it up in [`TOPICS`]. An unrecognized topic points back at
/// the bare `help` list rather than erroring.
pub(super) fn help_topic(topic: &str) -> String {
    command::parse(topic)
        .verb()
        .and_then(|verb| TOPICS.iter().find(|(v, _)| *v == verb))
        .map_or_else(
            || format!("no help for `{topic}` — type `help` for the command list"),
            |(_, body)| (*body).to_owned(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cli::shell::Flow;
    use crate::cli::shell::testenv::dispatch_one;

    #[test]
    fn help_lists_the_core_verbs() {
        let (flow, env) = dispatch_one("help");
        assert_eq!(flow, Flow::Continue);
        let joined = env.lines.joined();
        for verb in ["search", "info", "add", "upgrade", "apply", "quit"] {
            assert!(joined.contains(verb), "help text missing `{verb}`");
        }
    }

    #[test]
    fn help_topic_prints_the_single_command_detail() {
        let (flow, env) = dispatch_one("help add");
        assert_eq!(flow, Flow::Continue);
        let joined = env.lines.joined();
        assert!(joined.contains("add <sel…>"), "got: {joined}");
        // The detail names `add`'s alias and its resolution scope, which the
        // one-line overview doesn't.
        assert!(joined.contains("install"), "add topic omits its alias");
        // It's the topic, not the whole list — an unrelated verb's body is absent.
        assert!(!joined.contains("Leave the shell"), "printed the full list");
    }

    #[test]
    fn help_topic_resolves_aliases() {
        // `discard` is an alias for `drop`; `help discard` shows drop's topic.
        let (_, env) = dispatch_one("help discard");
        assert!(env.lines.contains("drop <sel…>"), "got: {:?}", env.lines);
    }

    #[test]
    fn help_unknown_topic_points_back_at_help() {
        let (_, env) = dispatch_one("help frobnicate");
        assert!(
            env.lines
                .any(|l| l.contains("no help") && l.contains("frobnicate")),
            "got: {:?}",
            env.lines
        );
    }

    #[test]
    fn every_verb_has_a_help_topic() {
        // Guards TOPICS against drifting from the verb set: a new verb without a
        // topic (or a renamed one) fails here rather than printing "no help".
        for verb in Verb::ALL {
            assert!(
                TOPICS.iter().any(|(v, _)| v == verb),
                "no `help {}` topic",
                verb.name(),
            );
        }
    }
}
