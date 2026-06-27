# Plan: shell-like (REPL) UI for interactive `gaur`

Status: phases 1–2 implemented; cart / approval / apply (phase 3+) proposed.

## Goal

Replace the **wizard-like** interactive UX — fixed linear sequences of modal
`dialoguer` steps (fullscreen `MultiSelect` picker → `Confirm` gate → per-PKGBUILD
review loop) — with a **shell-like** REPL: a persistent prompt where the user
drives with typed word-commands against a long-lived session and a **staged
transaction with an approval gate**, instead of being walked through prompts in a
fixed order.

The headline flow is the upgrade procedure: `upgrade` refreshes the indexes and
stages the available upgrades; the user refines the set (`discard`/`add`),
approves the AUR packages (`review`/`approve`; repo packages auto-approve), and
runs `apply`. An `apply` interrupted or failed mid-build drops back to the
shell — not out of gaur — with the cart intact, so the user can `discard` the
offender and `apply` the rest.

```
$ gaur
gitaur shell — type `help` for commands, `quit` to leave
gaur> upgrade
:: refreshing AUR mirror + index … done (3.8s)
:: 14 upgrades staged — 11 repo (approved), 3 AUR (need review)
    1  core   glibc          2.40-1 → 2.41-1     approved
   ……
   12  aur    yay-bin        12.4-1 → 12.5-1     review
   13  aur    firefox-git    (vcs)               review
   14  aur    cuda           12.6-1 → 12.8-1     review
gaur> discard cuda                      # not today
gaur> add yubikey-personalization       # extra install, same transaction
gaur> review yay-bin firefox-git        # diff cycle: approve / skip / discard each
  ── yay-bin ── PKGBUILD diff …  [a]pprove [s]kip [d]iscard [v]iew [e]dit: a
  ── firefox-git ── PKGBUILD diff …                                      : a
gaur> approve yubikey-personalization   # approve without opening a diff
gaur> show
:: transaction — 13 package(s), +2 deps · all approved
   …change-set table with sizes + build-time + total…
gaur> apply
   …build + install, one sudo batch…
   ✗ firefox-git failed to build — dropped back to the shell
gaur> discard firefox-git
gaur> apply                             # retry the rest; firefox-git no longer staged
   …done…
gaur> quit
```

## Locked decisions (from review)

1. **Augment, don't replace, the flag CLI.** Bare interactive `gaur` opens the
   shell. Explicit `gaur -S…/-Ss…/-Si…/-Syu`, bare-term search, and all pacman
   pass-through keep their **current one-shot, scriptable** behavior unchanged.
   Non-interactive bare `gaur` (pipe / cron / `--noconfirm`) still does a single
   `-Syu` pass. The shell is *strictly* the interactive no-arg path.
2. **Words-only command vocabulary.** `search` `info` `add` `discard` `remove`
   `upgrade` `review` `approve` `show` `apply` `clear` `refresh` `help` `quit`.
   No pacman-letter clusters, no clap. Note the three distinct removal-ish verbs:
   `discard` un-stages from the cart, `remove` *stages an uninstall* (`pacman -R`),
   `clear` empties the whole cart.
3. **Staged transaction with an approval gate, run by `apply`.**
   `upgrade`/`add`/`discard`/`remove` build a pending set across many commands.
   Every staged package carries an approval state: **repo packages auto-approve;
   AUR packages need review by default (configurable).** `review`/`approve` move
   AUR items to approved. `apply` runs the build+install **only when every
   staged package is approved**, in one transaction. This unifies fresh-install
   and upgrade and subsumes today's `upgrade_loop`.
4. **`upgrade` is a procedure; `apply` is resumable.** `upgrade` refreshes the
   mirror+index, recomputes available upgrades, and seeds the cart with them. An
   `apply` interrupted (Ctrl+C) or failed mid-build drops back to the *shell*
   with the cart intact and the offending pkgbase badged, so the user can
   `discard` it and `apply` the rest — never restart the command.
5. **Selection by numbers *and* package names with wildcards.** Commands that
   present a list (`search`, `upgrade`, `show`) remember it; selector arguments
   accept numbers (`3`), ranges (`5-8`), names (`glibc`), and globs (`python-*`).
   Numbers/ranges index the last list. The *universe* a name/glob resolves
   against is verb-scoped: `add` resolves against the AUR index + sync DBs (you
   can add anything); `discard`/`review`/`approve` resolve against the **cart**
   (you act on what's staged); `approve *` means "every staged AUR package".
6. **`apply` is one atomic add+remove transaction** (target state) — a single
   native libalpm transaction carrying repo adds, AUR file adds, *and* removals,
   so "package(group) X replaces package(group) Y" lands without a window where
   neither is installed. See [Applying the transaction](#applying-the-transaction-one-atomic-addremove)
   for why pacman's CLI can't do this and how the native path gets there. Phased:
   the first cut uses pacman calls (which already make *declared* replaces atomic);
   the native combined commit follows.
7. **Tab completion is first-class**, not polish: context-aware completion of
   command verbs and package names from word one. See [Tab completion](#tab-completion).

## Why this fits the codebase

The no-arg upgrade path (`src/cli/upgrade_loop.rs`) is *already* a loop that
hoists the expensive once-per-session work (mirror fetch, index+secondary load,
`MirrorRepo`, metrics store) out of the iteration and only re-snapshots the
localdb per pass (`UpgradeSession` + `recompute_remaining`). The shell is the
**generalization** of that loop: the fixed recompute→pick→confirm→apply sequence
becomes a command dispatch loop, and the dialoguer multi-select table is replaced
by the cart + selector commands. Most of the existing machinery is reused as-is;
the wizard widgets are what go away.

| Reused unchanged | Becomes shell-native |
| --- | --- |
| `UpgradeSession` (load, `recompute_remaining`, `pkgbase_of`, `index`, `secondary`) | `dialoguer::MultiSelect` picker (`ui::select_upgrades`, `search::pick`) |
| `build::resolve_targets` / `apply_plan` / `cmd_install` (the apply engine) | the fixed confirm-then-review wizard order |
| `ui::change_set_table` + `PreviewMetrics` (the `show`/`apply` preview) | `cli/upgrade_loop.rs` `drive` loop (retired once the shell reaches parity) |
| `build::review::review` + a per-pkgbase `reviewed` set | |
| `mirror::cmd_refresh`, `alpm_db::open`/`open_synced`, metrics store | |
| `Error::Interrupted` / `Error::UserAbort`, makepkg SIGINT bail-to-table | |

Three current dead-ends dissolve under this model:

- **UPDATE_LOOP phase 4** (live inline dep-expansion + `v` review hotkey) was
  blocked because `dialoguer::MultiSelect` has no toggle-time or custom-key hook.
  In the shell, `show` *is* the live dep-expansion (re-resolve + print on demand)
  and `review <sel>` *is* the `v` hotkey — both plain commands, no custom picker.
- The **route-2 per-root dep nesting** the change-set doc deferred is now just a
  rendering choice we fully own.
- The per-pkg **approval state** the wizard couldn't express (its review was a
  blocking modal between fetch and build) becomes first-class cart state the user
  drives with `review`/`approve` in any order.

## The upgrade procedure

`upgrade [sel…]`:

1. **Refresh.** `mirror::cmd_refresh` (the `-Sy`: incremental fetch + index
   update) and reload the in-memory session index so subsequent `search`/`info`
   see the fresh data too. (The session index is therefore *re-loadable* across a
   session — on `upgrade`/`refresh` — not immutable as the first draft assumed.)
2. **Compute.** `UpgradeSession::recompute_remaining(devel)` → the repo + AUR
   upgrade candidates against the current localdb.
3. **Seed.** Stage every candidate into the cart (repo → auto-approved, AUR →
   needs-review per config), display as a numbered list with per-item status.
   With `sel…`, seed only the matching subset (numbers index the freshly computed
   list; names/globs match candidate names).
4. The user then refines and approves (below) and runs `apply`.

`add`/`apply` work **without** a prior `upgrade` too — `add firefox` stages a
fresh install into the same cart. `upgrade` is just the bulk-seed-with-available-
upgrades command; the cart and `apply` are general.

## Approval & review

Each staged item has an `Approval`: `Approved` or `NeedsReview`.

- **Repo packages → `Approved`** on staging (pacman owns their provenance;
  there's no PKGBUILD to read).
- **AUR packages → `NeedsReview`** by default, governed by config (the existing
  `review_default`: `prompt`/`always-show` ⇒ `NeedsReview`, `skip` ⇒
  auto-`Approved`). A clearer dedicated knob (`aur_approval = review | auto`) may
  replace it; TBD at implementation.

Moving an AUR item to `Approved`:

- **`review <sel…>`** — for each selected AUR pkgbase, run the existing diff
  review (`build::review::review`, PKGBUILD-or-diff-against-installed) as a cycle
  whose outcomes are **approve / skip / discard / view / edit**:
  - *approve* → `Approved`;
  - *skip* → leave `NeedsReview` (look later), move to the next;
  - *discard* → remove from the cart entirely.
- **`approve <sel…>`** — mark approved **without** opening a diff (the
  "I trust this one" shortcut). **`approve *`** approves every staged AUR item.

A per-pkgbase `reviewed: HashSet<PkgBase>` remembers diffs already approved this
session, so discarding and re-adding (or a post-failure retry) doesn't re-prompt.

## apply

`apply`:

1. **Gate.** Refuse while any staged item is `NeedsReview`, listing them
   (`needs review: firefox-git, cuda — run \`review\` or \`approve\``). Repo-only
   carts are always ready.
2. **Resolve + preview.** `build::resolve_targets` over the approved set →
   `ui::change_set_table` (roots + pulled-in deps + "will remove" rows + sizes +
   build-time), then a final `ui::confirm`.
3. **Run.** Build AUR (stratified, `apply_plan`), install repo + AUR (+ removals)
   — one transaction in the target state (see next section).
4. **Resume on failure.** Ctrl+C or a build failure folds the partial
   `RunReport` into the cart's `history`, badges the offending pkgbase, and
   **returns to the prompt** (via `Error::Interrupted` / the report's failed set)
   — the cart keeps everything not-yet-installed staged. The user `discard`s the
   offender (or fixes its PKGBUILD via `review … → edit`) and `apply`s again.
   Successfully-installed items drop out of the cart on the next recompute.

## Custom types

### `Cart` — the staged transaction

```rust
/// The pending transaction the shell builds up; run by `apply`. Not persisted —
/// quitting drops it (matches `upgrade_loop::SessionState`'s session-only stance).
struct Cart {
    /// Staged installs/upgrades (repo + AUR), each with its approval state.
    items: Vec<CartItem>,
    /// Packages staged for uninstall → `pacman -R` / `trans_remove_pkg` at apply.
    remove: Vec<PkgName>,
    /// PKGBUILDs approved this session — suppresses repeat diffs across
    /// discard/re-add and post-failure retries.
    reviewed: HashSet<PkgBase>,
    /// Failed/interrupted/skipped badges carried across apply runs, lifted from
    /// `upgrade_loop::SessionState`.
    history: SessionState,
}

struct CartItem {
    /// Carries the counterpart hint through expand → resolve → prepare exactly
    /// like `upgrade_loop::resolve_aur` (upgrade rows hint the foreign pkgname).
    target: build::Target,
    source: Source,        // Repo | Aur — decides auto-approval + apply lane
    approval: Approval,    // Approved | NeedsReview
}

enum Source { Repo, Aur }
enum Approval { Approved, NeedsReview }
```

### `Selector` — numbers + names + globs

```rust
/// One selector argument; `add`/`discard`/`review`/`approve`/`info` parse their
/// args into these. The universe a name/glob resolves against is supplied by the
/// caller (index+sync for `add`; the cart for `discard`/`review`/`approve`).
enum Selector {
    Index(usize),       // `3`        → current list / cart row
    Range(usize, usize),// `5-8`      → rows
    Name(String),       // `glibc`    → literal name, passed through
    Glob(Regex),        // `python-*` → anchored regex over the universe
}
```

Resolution is a pure function `resolve(args, list, universe) -> Vec<PkgTarget>` —
the single reusable core, unit-tested without I/O (**implemented in phase 2**;
see `src/cli/shell/selector.rs`). A glob that matches nothing warns rather than
erroring (shell-like); an out-of-range number/range is a hard error.

> Implementation note: phase 2 compiles globs to an anchored `regex::Regex`
> (reusing the existing `regex` dep) rather than pulling in `globset` — `*`→`.*`,
> `?`→`.`, everything else escaped.

### `Command` — the parsed verb

A small enum: `Search(Vec<SearchTerm>)`, `Info(Vec<String>)`, `Add(Vec<String>)`,
`Discard(Vec<String>)`, `Remove(Vec<String>)`, `Upgrade(Vec<String>)`,
`Review(Vec<String>)`, `Approve(Vec<String>)`, `Show`, `Apply`, `Clear`,
`Refresh`, `Help(Option<String>)`, `Quit`, plus `Empty` / `Unknown` / `Syntax`.
Argument-bearing cart verbs keep raw `String` tokens that the handlers feed to
`Selector`. Parsing is `shell-words` tokenization + a verb match — no clap.

## Dependencies

- **`rustyline`** (added, v18) for the line editor: history
  (`$XDG_STATE_HOME/gitaur/shell_history`), emacs keybindings, and a `Completer`
  (pending) over verbs + names.
- **`shell-words`** (added) for tokenizing the input line.
- Globs reuse the existing **`regex`** dep — no `globset`/`glob` added.

rustyline owns the terminal only while reading a line; during `apply` we're away
from the prompt, so `indicatif` bars and the existing review prompt work as today.

## Module layout

```
src/cli/shell.rs            run(): session hoist + REPL loop + rustyline wiring   [done]
src/cli/shell/command.rs    Command enum + parse() (shell-words → verb)           [done]
src/cli/shell/selector.rs   Selector enum + resolve()                             [done]
src/cli/shell/cart.rs       Cart + CartItem + apply() (resolve_targets/apply_plan/-R)  [phase 3]
src/cli/shell/complete.rs   rustyline Completer over verbs + the name universe    [pending]
```

The control flow is split like `upgrade_loop`'s `drive`/`LoopEnv`: a pure
`dispatch(cmd, &mut state, &mut env) -> Flow` core behind a `ShellEnv` trait, so
command sequencing (cart mutation, approval transitions, exit conditions) is
unit-testable with a scripted fake env — no mirror, picker, or build. (In place
since phase 1; grows methods per phase.)

## Wiring point

`src/cli/dispatch.rs::dispatch`, the interactive no-arg branch, calls
`shell::run(cfg, devel)` (done). Everything else in `dispatch` and all of
`cli::run`'s pre-scan is untouched — the "augment, keep flags" decision in one
line. `upgrade_loop.rs` stays until the shell reaches upgrade parity (phase 4),
then is deleted; until then the shell's `upgrade` bridges to it.

## Startup behavior

The shell starts **cheap**: load the existing on-disk index (for `search`/`info`)
and build the name universe — **no network refresh at startup**. Fetching belongs
to an explicit `upgrade` (or `refresh`), per the RFC: `gaur` → prompt instantly;
`upgrade` → refresh + stage. This also resolves the old "auto-stage on entry?"
question — entry stages nothing; `upgrade` is the deliberate first move.

## Signals

| Ctrl+C arrives during | Result |
| --- | --- |
| line editing at the prompt | rustyline returns `Interrupted`; clear the line, redraw prompt — **never exit** (done) |
| a `apply` build (`makepkg`) | existing `Error::Interrupted` bail: mark pkgbase interrupted, fold into the cart, **return to prompt** |
| Ctrl+D (EOF) at the prompt, or `quit`/`exit` | exit the shell cleanly (`Ok(0)`) (done) |

Same interrupt contract the loop already implements, with "the table" → "the
prompt" and the partial report folded into the cart instead of a loop session.

## Applying the transaction: one atomic add+remove

The motivating case: one package (or package *group*) replaces another, with no
window where the old set is gone but the new set isn't in yet — and one sudo
prompt, one progress UI.

**What pacman's CLI can and can't do.** A single `pacman -S <names>` or
`pacman -U <files>` *does* remove packages atomically **when the removal is
declared** — the new package's `conflicts=` / `replaces=` makes pacman pull the
conflicting/replaced installed package out **in the same transaction**. So the
common "`foo-bin` replaces `foo`", "`foo-ng replaces=foo`", and EOL-repo → AUR
transitions already work atomically, *provided the new package goes in via one
pacman call*. What the CLI **cannot** express is an **undeclared** remove+add
("uninstall group A and install unrelated group B as one transaction"): `pacman
-R A` and `pacman -S/-U B` are two transactions, and no single CLI call mixes
sync-repo adds (`-S name`) with local-file adds (`-U file`).

**libalpm can.** A single `alpm` transaction may register both additions
(`trans_add_pkg`, for syncdb packages *and* `pkg_load`'ed `.pkg.tar` files) and
removals (`trans_remove_pkg`) before one `trans_prepare` + `trans_commit`. This
is precisely the API gitaur **already drives read-only** in
`pacman::invoke::preflight_dash_u_inner` (`trans_init(NO_LOCK)` → `pkg_load` →
`trans_add_pkg` → `trans_prepare` → `trans_release`). The only missing pieces for
a real commit: take the DB lock instead of `NO_LOCK`, add the `trans_remove_pkg`
calls, `trans_commit` — and do it **with privilege**. This is also the direction
memory `feedback_native_libalpm_over_pacman` points ("`alpm` crate for DB
reads+writes … own progress UI; shell out only for the privileged final txn").

**The privilege boundary.** Committing writes `/var/lib/pacman` (root), and gitaur
runs unprivileged (it lets *pacman* escalate). The clean way to keep one-sudo: a
small **internal privileged subcommand** — `apply` serializes the prepared
transaction (syncdb add names + AUR file paths + remove names + flags) and
re-execs `<escalator> gaur __commit-txn <spec>`, which opens alpm, registers
adds+removes, prepares, commits, and owns the install progress UI. One
escalation, one transaction, full atomicity across repo + AUR + removals.

**Phasing this sub-feature:**

- *Interim (phase 3).* `apply` issues the existing pacman calls:
  `dispatch::run_repo_upgrade` / `pacman -S` for repo, `apply_plan`'s per-stratum
  `pacman -U` for AUR, `pacman -R` for removals. Declared replaces/conflicts are
  atomic within each call; an undeclared remove+add is two transactions bridged by
  the sudo cache. Honest, shippable, matches today.
- *Target (phase 6).* Replace the privileged step with the native combined
  `__commit-txn`, behind a `native_commit` config knob; flip the default once the
  container suite covers the add+remove and group-swap cases.

**Resolver note.** For the cart to *show* the removals a declared replace implies,
`show`/`apply` reuse the read-only `trans_prepare` in `invoke.rs`: prepare the
add set, read back `ConflictingDeps` / replaced packages, list them as
"will remove" rows — honest preview even in the interim phase.

## Tab completion

Context-aware completion from the first keystroke, via a rustyline `Completer`
(`src/cli/shell/complete.rs`), **positional**:

| Cursor position | Completes to |
| --- | --- |
| first word | command verbs + `help` topics |
| arg of `search` / `add` / `info` | package names — AUR pkgbases/pkgnames + sync-DB names (the full universe) |
| arg of `discard` / `review` / `approve` / `upgrade` | names currently **in the cart** (the relevant small set) |
| arg of `help` | command verbs |
| a numeric token | no completion (numbers index the current list) |

**Name source + speed.** Phase 2 already builds the sorted, de-duplicated name
universe once at session start (AUR pkgnames + pkgbases + sync-DB names) for glob
resolution. The completer answers each Tab with a binary search for the prefix
range, capped (e.g. 200 shown, "+N more"). It shares that universe (and the cart)
with the `Selector` resolver so "what Tab offers" and "what the verb accepts"
never drift. An optional `Hinter` can ghost-suggest from history later.

## Phasing

Each phase is independently shippable and leaves the flag CLI fully working.

1. **REPL skeleton. — DONE.** rustyline loop, `shell-words` parse, `Command`
   enum, `help`/`quit`/Ctrl-C/Ctrl-D, persistent history, and the `ShellEnv` +
   pure `dispatch` split with scripted-fake unit tests. Wired at the no-arg
   interactive branch. Bare interactive `gaur` enters the shell unconditionally
   (no env gate); `upgrade` bridges to `upgrade_loop` for now; the cart verbs are
   stubs. (`src/cli/shell.rs` + `command.rs`.)
2. **Read-only commands + selector core. — DONE (tab-completion pending).**
   Session hoisted via `UpgradeSession`; `search` prints a numbered list the
   session remembers; `info` resolves number/range/name/glob via the `Selector`
   core against the remembered list + the name universe. `SearchTerm` for query
   patterns, `PkgTarget` for `info`/`-Si` targets — threaded through
   `search_sync`/`cmd_search`/`cmd_info`. (`shell.rs` + `selector.rs` + `command.rs`.)
   **Pending:** the rustyline `Completer`.
3. **Cart + approval + apply (interim, pacman calls).** `add`/`discard`/`remove`/
   `clear`/`show` build the `Cart` with per-item `Approval`; `review <sel>`
   (approve/skip/discard cycle over `build::review::review`) and `approve [*]`
   move AUR items to approved; `apply` gates on all-approved, then preview
   (`change_set_table` incl. "will remove" rows) → `confirm` → `apply_plan` + repo
   `pacman -S` + `pacman -R`; a failed/Ctrl-C'd build folds into the cart and
   returns to the prompt. The felt payload — staged installs with review in the
   shell.
4. **`upgrade` procedure in the shell.** `upgrade [sel…]` refreshes + reloads the
   session, recomputes candidates, seeds the cart (repo approved / AUR
   needs-review); port the cost overlay (sizes, build-time, `built` tag). Retire
   `upgrade_loop.rs` and the dialoguer multi-select table.
5. **Polish.** Tab-completion (verbs + cart/universe), `refresh`, per-root dep
   nesting in `show`, history `Hinter`, `help <topic>`, config knobs
   (`aur_approval`, prompt/history).
6. **Native combined commit (atomic add+remove).** Internal `__commit-txn`
   privileged subcommand: one libalpm transaction over repo adds + AUR file adds +
   removals, owning the install progress UI. Reuses `invoke.rs`'s transaction
   machinery. Behind `native_commit`; flip default once the container suite covers
   add+remove / group-swap. Satisfies decision 6 fully.

## Testing

Mirrors the existing two-tier philosophy (`docs/TESTING.md`) and the loop's seams:

- **Unit** — `command::parse`, `selector::resolve`, and the `dispatch` core via a
  scripted `ShellEnv` fake: cart mutation (`add`/`discard`), approval transitions
  (`review`/`approve`/`approve *`), the `apply` gate refusing while items need
  review, and fold-on-failure keeping the cart. (The `drive`/`FakeEnv` pattern.)
- **Container e2e** — drive the real REPL under a PTY via the `pty-harness`
  dev-crate (precedent: `loop_built_tag_e2e`). Script: `upgrade` → `discard` →
  `approve *` → `apply` asserting installed state; a Ctrl-C-bails-to-prompt
  case; and a build-failure-then-discard-then-apply case.

## Out of scope (this iteration)

- A fullscreen TUI (ratatui) — the decision is line-oriented REPL.
- Looping/changing the explicit `-Syu` flag path — keeps its one-shot picker.
- Scriptable shell input files / a `-c "command"` mode (the dispatch core is pure
  enough to add it later).
- Cross-user metric sharing (already out of scope in UPDATE_LOOP).

## Code anchors

| File | Anchor | Role |
| --- | --- | --- |
| `src/cli/shell.rs` | `dispatch`/`ShellEnv`/`run`, `State`, `ListItem` | REPL core (done); grows the cart + apply env methods |
| `src/cli/shell/selector.rs` | `resolve` | numbers/ranges/names/globs → targets (done) |
| `src/cli/upgrade_loop.rs` | `UpgradeSession`, `recompute_remaining`, `resolve_aur`, `preview*`, `candidate_metrics`, `SessionState::fold` | reuse for `upgrade`/`show`/`apply`; retire in phase 4 |
| `src/cli/search.rs` | `Row`, `label`, `picked` | reused for the shell's `search` rows (done) |
| `src/build.rs` | `resolve_targets`, `apply_plan`, `cmd_install`, `Target::{with_hint,bare}`, `RunReport` | the apply engine + fold |
| `src/build/review.rs` | `review()` → approve/skip/discard/view/edit | the `review` command's diff cycle |
| `src/ui/change_set.rs` | `change_set_table` | the `show`/`apply` preview |
| `src/ui/tables.rs` | `select_upgrades` | **flag path only** — the shell never calls it |
| `src/pacman/invoke.rs` | `preflight_dash_u_inner`, `exec_pacman`, `confirm_escalation` | native-txn template for `__commit-txn`; "will remove" preview |
| `src/error.rs` | `Interrupted`, `UserAbort` | apply bail-to-prompt + decline |
| `src/config.rs` | `review_default` | default AUR approval (→ `aur_approval`) |
