# Plan: shell-like (REPL) UI for interactive `aurox`

Status: phases 1–4 implemented; phase 5a (one unified renderer) + 5b (sorted-cart
invariant) **done**; phase 5c **done** — **tab-completion** (the rustyline
`Completer`), the **`refresh`** command, a completion-driven **`Hinter`** (dimmed
inline type-ahead, colour-mode aware), **`help <topic>`** (per-command detail,
aliases resolved through `command::parse`), and the **`aur_approval`** config knob
are all in. Remaining: the native combined commit (phase 6), plus optional
prompt/history-size config knobs.

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
shell — not out of aurox — with the cart intact, so the user can `discard` the
offender and `apply` the rest.

```
$ aurox
aurox shell — type `help` for commands, `quit` to leave
aurox> upgrade
:: refreshing AUR mirror + index … done (3.8s)
:: 14 upgrades staged — 11 repo (approved), 3 AUR (need review)
    1  core   glibc          2.40-1 → 2.41-1     approved
   ……
   12  aur    yay-bin        12.4-1 → 12.5-1     review
   13  aur    firefox-git    (vcs)               review
   14  aur    cuda           12.6-1 → 12.8-1     review
aurox> discard cuda                      # not today
aurox> add yubikey-personalization       # extra install, same transaction
aurox> review yay-bin firefox-git        # diff cycle: approve / skip / discard each
  ── yay-bin ── PKGBUILD diff …  [a]pprove [s]kip [d]iscard [v]iew [e]dit: a
  ── firefox-git ── PKGBUILD diff …                                      : a
aurox> approve yubikey-personalization   # approve without opening a diff
aurox> show
:: transaction — 13 package(s), +2 deps · all approved
   …change-set table with sizes + build-time + total…
aurox> apply
   …build + install, one sudo batch…
   ✗ firefox-git failed to build — dropped back to the shell
aurox> discard firefox-git
aurox> apply                             # retry the rest; firefox-git no longer staged
   …done…
aurox> quit
```

## Locked decisions (from review)

1. **Augment, don't replace, the flag CLI.** Bare interactive `aurox` opens the
   shell. Explicit `aurox -S…/-Ss…/-Si…/-Syu` and all pacman pass-through keep
   their **current one-shot, scriptable** behavior unchanged.
   *Revised:* interactive **bare-term search** (`aurox <term>…`) now opens the
   shell too, seeded with that `search` (identical to typing `search <term>…` at
   the prompt) — the old `MultiSelect` picker is gone, so the REPL is the one
   interactive surface. Non-interactively (pipe / `--noconfirm`) `aurox <term>…`
   stays a one-shot **ranked listing** that installs nothing. Non-interactive
   bare `aurox` (pipe / cron / `--noconfirm`) still does a single `-Syu` pass.
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
5. **Selection by numbers *and* package names with wildcards *and* repo names.**
   Commands that present a list (`search`, `upgrade`, `show`) remember it;
   selector arguments accept numbers (`3`), ranges (`5-8`), names (`glibc`),
   globs (`python-*`), and **repo names** (`aur`, `core`, `extra`, …) that select
   every row from that repo — `drop aur` un-stages all AUR rows, `add extra`
   stages every `extra` row from the last list. Numbers/ranges index the last
   list. The *universe* a name/glob/repo resolves against is verb-scoped: `add`
   resolves against the last list + AUR index + sync DBs (you can add anything);
   `discard`/`review`/`approve` resolve against the **cart** (you act on what's
   staged); `approve *` means "every staged AUR package". A repo-name token only
   expands when something in the verb's scope is from that repo, so a real
   package sharing a repo's name still resolves normally otherwise.
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

*(Design rationale, written when the no-arg `upgrade_loop` still existed. Phase
4 retired that driver; its reusable machinery moved to `src/cli/shell/upgrade.rs`.
The derivation below is the original motivation, kept as history.)*

The no-arg upgrade path (then `src/cli/upgrade_loop.rs`) was a loop that
hoisted the expensive once-per-session work (mirror fetch, index+secondary load,
`MirrorRepo`, metrics store) out of the iteration and only re-snapshotted the
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
2. **Confirm — no table re-draw.** `show` already rendered the full resolved
   change-set table (see [Unifying the show / apply
   tables](#unifying-the-show--apply-tables)), so `apply` does **not** re-draw it
   — the user just curated that exact list, and `show`/`status` reprints it on
   demand. `apply` resolves the approved set (`build::resolve_targets`) only to
   recompute the **one-line cost summary** (`13 install, +2 deps, 1 remove ·
   ~3.4 GiB · ~22m build`) and takes a single `ui::confirm` before the
   irreversible build/sudo step. `show` is where you look; `apply` is where you
   commit.
3. **Run.** Build AUR (stratified, `apply_plan`), install repo + AUR (+ removals)
   — one transaction in the target state (see next section).
4. **Resume on failure.** Ctrl+C or a build failure folds the partial
   `RunReport` into the cart's `history`, badges the offending pkgbase, and
   **returns to the prompt** (via `Error::Interrupted` / the report's failed set)
   — the cart keeps everything not-yet-installed staged. The user `discard`s the
   offender (or fixes its PKGBUILD via `review … → edit`) and `apply`s again.
   Successfully-installed items drop out of the cart on the next recompute.

## Unifying the `show` / `apply` tables

Phases 3–4 left the shell with **two** divergent package tables:

| | `show` / `status` / `upgrade` | `apply` (upgrade cart) |
| --- | --- | --- |
| renderer | `RealEnv::render_cart` (`src/cli/shell.rs`) | `ui::change_set_table` (`src/ui/change_set.rs`) |
| order | cart staging order | sorted: repo → bump-severity → name |
| version | naive full-string `old`(red) `→` `new`(green) | verdiff: common prefix dimmed, changed suffix colored by bump kind, aligned via `col_widths` |
| columns | `№  repo  approval  name  old → new  age` | `repo  name  old → new  size  build-time` + pulled-in deps + batch total |
| resolves deps? | no (cart roots only) | yes (`resolver::resolve` → repo/AUR dep rows) |

Two problems with this split:

1. **`apply` re-draws a table the user just curated.** They built the cart with
   `upgrade`/`add`/`drop`/`review` and watched it in `show`; making `apply`
   reprint a (differently-shaped!) table is noise. If they want to re-check, that's
   what `status` is for.
2. **The two renderers look nothing alike** — different sort, different version
   coloring, different columns — so the cart the user curated and the thing
   `apply` shows read as two unrelated screens.

### Decision: one table, owned by `show`; `apply` only confirms

There is **one** transaction-table renderer, rendered by `show`/`status`/`upgrade`.
`apply` stops drawing it and gates on a one-line cost summary instead (see `apply`
step 2 above). `show` becomes the genuine pre-commit preview — it resolves the
staged set so the user sees **the whole change set** (roots + pulled-in deps +
removals) with cost, which is UPDATE_LOOP goal #5 finally landing in the shell.

The unified table is the **union** of the two renderers' good parts:

| Feature | Kept from |
| --- | --- |
| `№` (row number) + `approval` columns | `render_cart` |
| AUR `last-modified` age column | `render_cart` |
| concrete `RepoName` repo column (yay-hashed color) | `render_cart` |
| **sort by repo, then name** | `change_set_table` (drop the bump-severity middle key — decision below) |
| verdiff version coloring + column alignment | `change_set_table` / `tables::render_row` |
| **size** column + batch total | `change_set_table` |
| build-time column + total, `built` tag | `change_set_table` |
| pulled-in deps block (`(install)`/`(build)`) | `change_set_table` |
| "will remove" rows | `change_set_table` (read-back still phase 6) |

Resulting layout (roots numbered + approval-tagged; deps indented, unnumbered):

```
aurox> show
:: transaction — 3 install, +2 deps, 1 remove · all approved
   1  core   approved  glibc      2.40-1 → 2.41-1     12.00 MiB
   2  aur    approved  yay-bin    12.4-1 → 12.5-1      ~9.00 MiB  (3d ago)
   3  aur    review    cuda       12.6-1 → 12.8-1      ~3.00 GiB  (1d ago)  ~18m build
-> pulls in:
        gcc13          (install)   50.00 MiB
        nvidia-utils   (build)           ~?            ~4m build
-> will remove:
        old-cuda
-> total  ~3.07 GiB   ~22m build
```

### Open decisions, with recommended defaults

- **Sort key.** Recommend **repo → name** (per the request); drop the
  bump-severity middle key `change_set_table` uses today. Severity sorting fights
  the user's "find this package in the list" mental model and is the kind of thing
  the per-row color already signals.
- **Number ↔ selector consistency.** Once `show` numbers rows in sorted order,
  `drop 3` must index *that* order, not staging order. The simplest fix is to
  **keep the cart's `Vec<CartItem>` sorted** (repo-rank → name) as its invariant,
  not to compute a sorted *view* on the side: then the displayed `№` is literally
  the vector index, and `resolve_against_cart` already indexes that same vector,
  so the two **can't diverge by construction** — no second order to keep in sync.
  (Today they agree precisely because both iterate the `Vec` in staging order;
  this keeps that property, just under a sorted invariant.) `Cart::add` /
  `upgrade`-seed insert in sorted position (or push-then-resort per batch — the
  cart is tiny). Staging order is dropped, and nothing depends on it:
  `install_targets`, `pending_review`, `repo_upgrades`, and the resolver are all
  order-insensitive. Reusing `repo_rank` for the sort key makes the cart's order
  identical to the table's column sort. **Tradeoff:** a sorted-insert `add` can
  shift the numbers of existing rows (append wouldn't), so the habit is `show`
  then numeric `drop`; `drop <name>`/`<repo>`/`<glob>` sidestep numbering
  entirely.
- **Resolve on every `show`?** Yes — `show` resolves the staged set to surface
  deps + sizes. It's one resolve (what `apply` does anyway); cache the resolved
  `Plan` keyed on cart contents and invalidate on any cart mutation so repeated
  `show`s are free. **Graceful degradation:** if resolve fails (unknown target,
  mirror gap), fall back to the flat cart rows (today's `render_cart` output) plus
  a warning — `show` must never error out.
- **Install vs upgrade row shape.** The renderer must handle both a fresh install
  (no `old`, render `→ new` or just `new`, no verdiff split) and an upgrade
  (`old → new`, verdiff). Today fresh-install carts dodge this by using the `-S`
  pipeline's `print::plan` table at apply; unifying means the row model carries an
  `Option<old_ver>` and `render_row` degrades to the install shape when it's
  `None`. This also retires the fresh-vs-upgrade branch in `apply`.

### Implementation shape

Fold both renderers into one in `src/ui/change_set.rs` (or a renamed
`ui::transaction`): extend its row model with the `№`/`approval`/`age` cells and
the install-vs-upgrade `Option<old_ver>`, keep its sort/verdiff/size/time
machinery. Retire `RealEnv::render_cart`'s bespoke table — `show` now feeds the
cart + the resolved `Plan` + the cost overlay (`upgrade::{preview_metrics,
synced_pac}`, already built) into the one renderer. `apply` drops its
`change_set_table` call and renders only the summary line (reuse the existing
`batch_size_total` / `batch_time_total`). The `-Qu` / `-Syu` **flag** path keeps
`tables::upgrade_table` + `render_row` untouched — it has no cart, number, or
approval concept, and `render_row` stays the shared version-rendering primitive
both tables call.

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
  (`$XDG_STATE_HOME/aurox/shell_history`), emacs keybindings, and the
  `ShellHelper` `Completer` over verbs + names (phase 5c).
- **`shell-words`** (added) for tokenizing the input line.
- Globs reuse the existing **`regex`** dep — no `globset`/`glob` added.

rustyline owns the terminal only while reading a line; during `apply` we're away
from the prompt, so `indicatif` bars and the existing review prompt work as today.

## Module layout

```
src/cli/shell.rs            run(): session hoist + REPL loop + rustyline wiring   [done]
src/cli/shell/command.rs    Command enum + parse() (shell-words → verb)           [done]
src/cli/shell/selector.rs   Selector enum + resolve()                             [done]
src/cli/shell/cart.rs       Cart + CartItem + Source/Approval/ApplyOutcome         [done]
src/cli/shell/upgrade.rs    refresh+reload, cost-overlay preview (ex-upgrade_loop) [done]
src/cli/shell/complete.rs   rustyline Completer over verbs + the name universe    [done]
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
to an explicit `upgrade` (or `refresh`), per the RFC: `aurox` → prompt instantly;
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
is precisely the API aurox **already drives read-only** in
`pacman::invoke::preflight_dash_u_inner` (`trans_init(NO_LOCK)` → `pkg_load` →
`trans_add_pkg` → `trans_prepare` → `trans_release`). The only missing pieces for
a real commit: take the DB lock instead of `NO_LOCK`, add the `trans_remove_pkg`
calls, `trans_commit` — and do it **with privilege**. This is also the direction
memory `feedback_native_libalpm_over_pacman` points ("`alpm` crate for DB
reads+writes … own progress UI; shell out only for the privileged final txn").

**The privilege boundary.** Committing writes `/var/lib/pacman` (root), and aurox
runs unprivileged (it lets *pacman* escalate). The clean way to keep one-sudo: a
small **internal privileged subcommand** — `apply` serializes the prepared
transaction (syncdb add names + AUR file paths + remove names + flags) and
re-execs `<escalator> aurox __commit-txn <spec>`, which opens alpm, registers
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
   interactive branch. Bare interactive `aurox` enters the shell unconditionally
   (no env gate); `upgrade` bridges to `upgrade_loop` for now; the cart verbs are
   stubs. (`src/cli/shell.rs` + `command.rs`.)
2. **Read-only commands + selector core. — DONE (tab-completion pending).**
   Session hoisted via `UpgradeSession`; `search` prints a numbered list the
   session remembers; `info` resolves number/range/name/glob via the `Selector`
   core against the remembered list + the name universe. `SearchTerm` for query
   patterns, `PkgTarget` for `info`/`-Si` targets — threaded through
   `search_sync`/`cmd_search`/`cmd_info`. (`shell.rs` + `selector.rs` + `command.rs`.)
   The rustyline `Completer` over this universe landed in phase 5c.
3. **Cart + approval + apply (interim, pacman calls). — DONE.**
   `add`/`drop`(alias `discard`)/`remove`/`clear`/`show` build the `Cart` with
   per-item `Source` + `Approval`; `review <sel>` runs `build::review::review`
   (approve/skip/abort) and `approve [*]` moves AUR items to approved without a
   diff; `apply` gates on all-approved, then runs the install half through the
   existing `-S` pipeline (`install_with_index` → plan table → conditional
   confirm → `apply_plan`) plus `pacman -R` for removals. A clean run clears the
   applied rows; a declined plan keeps the cart; a failed/interrupted build
   returns to the prompt with the cart intact for `drop`-and-retry. Cart staging,
   the approval gate, and the apply-clears/keeps logic are unit-tested behind the
   `ShellEnv` fake; the verbs are methods on `State`. Landed in
   `src/cli/shell/cart.rs` + the cart arms in `src/cli/shell.rs`.

   **As implemented (deviations from the sketch above):**
   - The apply preview is the `-S` pipeline's `print::plan` table (+ its
     `only_requested` confirm gate), **not** `ui::change_set_table` — that table
     is upgrade-shaped (`PkgUpgrade` roots with old→new), so it belongs to the
     phase-4 `upgrade`-seeded cart, not phase-3 fresh installs.
   - `review`/`approve` record the approved pkgbase in `Cart::reviewed`, which
     `apply` threads into the build pipeline so an approved AUR root isn't
     re-prompted. Pulled-in AUR **dependencies** (not staged roots) still get the
     normal `review_default`-driven review at build time — an honest interim.
   - "will remove" preview rows (read back from a `trans_prepare`) are **not**
     wired yet; removals just list as `pacman -R` targets. Order is install-then-
     remove (two transactions); the atomic combined commit is still phase 6.
   - `review_default` finally drives behaviour here: `"skip"` ⇒ AUR auto-approve
     (`AurApproval::Auto`), else needs-review. The dedicated `aur_approval` knob
     is still TBD.
   - Coarse `add`-time classification prefers Repo on a name in both sync + AUR
     (matches the resolver's own ordering); it only sets the approval policy and
     the `show` label — the real routing is the resolver's at `apply`.
4. **`upgrade` procedure in the shell. — DONE.** `upgrade [sel…]` refreshes +
   reloads the session in place (so `search`/`info`/classification see fresh
   data), recomputes candidates, and seeds the cart (repo approved / AUR
   needs-review per config). `CartItem` gained `upgrade: Option<PkgUpgrade>` —
   AUR upgrade rows hint their foreign pkgname (the loop's `resolve_aur` trick),
   and repo upgrade rows route through a **partial `pacman -Syu`** lane
   (`--ignore` every repo candidate the user didn't stage) instead of `pacman
   -S`. `apply` branches: a pure fresh-install cart keeps the phase-3 `-S`
   pipeline; an upgrade cart resolves the AUR/build half once, renders the
   ported cost-overlay `change_set_table` preview (sizes from the synced db,
   build-time + `built` from the metrics store), takes one confirm, then runs
   the `-Syu` repo lane + `apply_plan` + removals. The `upgrade_loop` driver +
   dialoguer picker are gone; its reusable helpers (preview, metrics overlay,
   the synced/system snapshots) moved to `src/cli/shell/upgrade.rs`. The
   single-shot `-Syu` flag path keeps its own `ui::select_upgrades` picker.

   **As implemented (deviations / deferred):**
   - The cost overlay lands in the **apply preview**, not the `upgrade` list (the
     shell has no picker to carry per-row cost cells) — `upgrade`/`show` render a
     colored, column-aligned table: `№  repo  approval  name  old → new  (age)`.
     The `repo` cell is the **concrete** sync-DB (`core`/`extra`/…, yay-style
     hashed color) rather than the coarse `repo`/`aur`; the `name` and `old → new`
     versions are separate aligned columns; and AUR rows carry a dimmed "last
     modified" age (`(3d ago)`) from the pkgbase's branch-tip commit time. The
     table body renders behind the `ShellEnv::render_cart` seam (color + width
     math + wall-clock age are I/O-shaped), while `show`'s header + approval
     summary stay in the pure dispatch core. Repo names are a typed
     `names::RepoName`, and the table aligns via a `Width`/`Colored`/`Cell`
     cluster so padding is on visible width, not byte length. **This bespoke
     `render_cart` table is superseded by the unified renderer** — see
     [Unifying the show / apply tables](#unifying-the-show--apply-tables) (phase
     5): `show` adopts `change_set_table`'s sort/verdiff/size while keeping the
     number/approval/age columns, and `apply` stops re-drawing a table.
   - Repo `repo_skipped` (the `--ignore` set) is **recomputed** at apply from the
     live candidate set minus the staged repo upgrades, so a stale cart can't pin
     the wrong packages.
   - A cart mixing fresh `add`s with `upgrade` rows applies correctly, but the
     change-set preview's roots are the upgrade rows; fresh installs show under
     the pulled-in/dep section.
5. **Polish + table unification.** The [unified `show`/`apply`
   table](#unifying-the-show--apply-tables) is the headline item:
   - *5a — one renderer. **DONE.*** `render_cart` + `change_set_table` are folded
     into one renderer in `src/ui/change_set.rs`: [`ui::transaction_table`] takes
     a `TxnRoot` row model (number/approval/age columns over the
     sort/verdiff/size/time machinery; `Option<old_ver>` for
     install-vs-upgrade) and *returns* its lines as a typed `ui::Table`. `show`
     resolves the staged set (graceful fallback to flat rows when resolve fails)
     and renders it via the `ShellEnv::render_cart` seam. `apply` no longer draws
     a table — both the fresh-install and upgrade branches collapsed into one
     `resolve_targets` → [`ui::cost_summary`] one-liner → single confirm →
     `apply_plan` (gate `AlreadyConfirmed`), so the `-S` pipeline's `print::plan`
     table is bypassed at apply time too (the "full de-table"). The rendering
     primitives are typed end-to-end: `Width`/`Cell`/`Paint`/`Fade` for layout,
     `Bytes`/`Precision`/`BuildTerm`/`Duration` for the totals, `RepoRank` for the
     sort key, `StageResult`/`UnstageResult` for cart mutations.
   - *5b — order consistency. **DONE.*** `Cart::add` keeps the `items` `Vec`
     sorted (`RepoName::rank()` → repo → spec) as an invariant, so the displayed
     `№` *is* the vector index `resolve_against_cart` addresses — they can't drift.
   - *5c — the rest.* **Tab-completion** (verbs + cart/universe) **DONE** — the
     rustyline `Completer` in `src/cli/shell/complete.rs` is context-aware and
     positional (verbs for word 1 / `help`; the name universe for
     `search`/`add`/`info`/`remove`; the cart for `drop`/`review`/`approve`/
     `upgrade`; nothing for a numeric token or a no-arg verb), recovering the
     active verb by re-parsing the pre-cursor line so aliases resolve for free.
     The universe is shared with the session by `Rc<[PkgTarget]>` and the cart
     snapshot is re-synced after each command, so completion and the selector
     resolver can't drift. **`refresh` DONE** — re-fetches the mirror + reloads
     the session (fresh data for `search`/`info`/`upgrade`/completion) without
     touching the cart. **`Hinter` DONE** — a completion-driven type-ahead
     (`ShellHelper::hint_for`, sharing the `Completer`'s sources): command
     positions (word 1 / `help <topic>`) always suggest the first matching verb;
     package positions suggest the **longest common prefix** of all matching
     names (the part every candidate agrees on, so it's certain even when the
     full name is still ambiguous — `gtk-vnc*` from `gtk-vn` → `c`; a divergent
     next char shows nothing). Over the sorted universe the common prefix is just
     the first/last match's, found with two binary searches — no scan of the
     matching range. Dimmed via `highlight_hint`; the editor's `ColorMode`
     follows the session's `--color`, so `never` renders it plain. **`help <topic>` DONE** — `help <command>` prints a per-verb detail
     from the `TOPICS` table (aliases resolved through `command::parse`); a
     `every_verb_has_a_help_topic` test guards the table against verb drift.
     **`aur_approval` DONE** — a typed `Option<AurApproval>` config knob
     (`review`/`auto`) resolved by `AurApproval::from_config`; unset defers to
     the legacy `review_default == "skip"` behaviour. *Remaining:* optional
     prompt-string / history-size knobs.
   ("will remove" rows read back via `trans_prepare` ride with phase 6.)
6. **Native combined commit (atomic add+remove).** Internal `__commit-txn`
   privileged subcommand: one libalpm transaction over repo adds + AUR file adds +
   removals, owning the install progress UI. Reuses `invoke.rs`'s transaction
   machinery. Behind `native_commit`; flip default once the container suite covers
   add+remove / group-swap. Satisfies decision 6 fully.

## Backlog — shell UX refinements (post-5c)

Found while using the shell; none block the phasing above. **All three DONE.**

1. **`upgrade` should refresh on a TTL, not every time. — DONE.** `upgrade` now
   defers its mirror fetch to `Config::refresh_max_age_secs` (default 3600s,
   previously defined-but-unread). `upgrade::refresh_and_reload` takes a typed
   `FetchPolicy`: `upgrade` passes `WhenStale` (skip the fetch when the last one
   is younger than the TTL, still reloading the in-memory session from disk),
   `refresh` passes `Always` (force a fetch, TTL-ignored); `-Syy` keeps forcing a
   full re-clone via `cmd_refresh`'s `force_reclone`. The last-fetch timestamp is
   a small stamp file (`paths::fetch_stamp_path` → `state_dir()/last-fetch`)
   written by `mirror::cmd_refresh` on every successful refresh and read by
   `mirror::last_fetch_age` — *not* an artifact mtime, because gix writes no
   `FETCH_HEAD`, `packed-refs` is rewritten only every ~2000 fetches, and the
   index/commit-graph are touched only when refs changed, so nothing on disk
   reliably records the common no-op fetch. A missing/garbled stamp reads as
   stale (fetch); a future stamp (clock skew) reads as a zero age (skip). Since
   `cmd_refresh` does both the AUR fetch *and* the repo-db sync, a TTL-fresh
   `upgrade` skips both network round-trips — the whole point being "no network
   on a just-refreshed `upgrade`"; `refresh` or TTL expiry restores freshness.
2. **`add` / `drop` should print the whole cart. — DONE.** After a *successful*
   cart-changing `add`/`drop`/`remove` (i.e. something actually staged/unstaged —
   a no-op `add nope` or `drop notincart` stays quiet), the dispatch core calls
   `State::show`, reprinting the full header + table + approval summary, so the
   current transaction is always on screen without typing `show`. The per-item
   acknowledgments (`staged foo (aur)`, `dropped bar`, the exceptional
   already-staged/unknown notes) are kept — they confirm each action and the
   `shell_cart_e2e` PTY driver asserts on `staged …`. `clear` keeps its terse
   `cart cleared` (reprinting an empty cart would just say "cart is empty").
   `approve`/`review` aren't in scope here (they don't change the package set);
   the user's next `show` reflects them — and is now a cache hit (see 3).
3. **Re-resolve on change, not on every `show`. — DONE.** `RealEnv` caches the
   expensive package-set-dependent half of the `show` view (`ResolvedTxn`: the
   `synced_pac` size snapshot, the pulled-in dep rows, and the build-time
   overlay) in `Option<CachedTxn>`, keyed by a `TxnKey` over the staged install
   targets + removal names (**approval excluded** — it changes only the rendered
   cell, not the resolution). `render_cart` → `transaction_view` → `ensure_view`
   re-resolves only on a key miss; the cache is cleared on `reload`
   (`upgrade`/`refresh` — upstream data moved) and at the start of `apply` (the
   installed set may move). So a package-set mutation resolves **once** (the
   reprint from item 2 is that one resolve), repeated `show`s are free, and
   `approve`/`review` followed by `show` re-derive only the approval-bearing root
   rows from the live cart against the cached snapshot — no re-resolve. The plan
   itself isn't cached (only the dep rows/overlay derived from it; `apply`
   resolves its own live plan). The graceful flat-row fallback on a resolve
   failure is unchanged.

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
| `src/cli/shell.rs` | `dispatch`/`ShellEnv`/`run`, `State`, `ListItem`, `RealEnv::render_cart` | REPL core (done); `render_cart` is the bespoke `show` table the phase-5 unification retires |
| `src/cli/shell/selector.rs` | `resolve` | numbers/ranges/names/globs/**repo** → targets (done) |
| `src/cli/shell/complete.rs` | `ShellHelper`, `candidates`, `arg_kind` | context-aware tab-completion: verbs / universe / cart per position (done) |
| `src/cli/shell/cart.rs` | `Cart` (`Vec<CartItem>`), `add`, `repo_label` | phase 5b keeps the `Vec` sorted (repo-rank → name) so `№` == index for `resolve_against_cart` |
| `src/cli/shell/upgrade.rs` | `refresh_and_reload`, `preview`, `preview_metrics`, `system_pac`/`synced_pac` | refresh+reload + the cost-overlay preview; phase 5 moves `preview`/overlay behind `show`, not `apply` |
| `src/build/upgrade.rs` | `UpgradeSession::{recompute_remaining,pkgbase_of}` | recompute candidates for `upgrade` |
| `src/cli/search.rs` | `Row`, `label`, `picked` | reused for the shell's `search` rows (done) |
| `src/build.rs` | `resolve_targets`, `apply_plan`, `cmd_install`, `Target::{with_hint,bare}`, `RunReport` | the apply engine + fold |
| `src/build/review.rs` | `review()` → approve/skip/discard/view/edit | the `review` command's diff cycle |
| `src/ui/change_set.rs` | `change_set_table`, `batch_size_total`/`batch_time_total` | grows into the **one** unified transaction renderer (phase 5a); totals feed `apply`'s summary line |
| `src/ui/tables.rs` | `render_row`, `col_widths`, `sort_for_display` | shared verdiff version-rendering primitive both tables call; unchanged for the `-Qu`/`-Syu` flag path |
| `src/ui/tables.rs` | `select_upgrades` | **flag path only** — the shell never calls it |
| `src/pacman/invoke.rs` | `preflight_dash_u_inner`, `exec_pacman`, `confirm_escalation` | native-txn template for `__commit-txn`; "will remove" preview |
| `src/error.rs` | `Interrupted`, `UserAbort` | apply bail-to-prompt + decline |
| `src/config.rs` | `review_default` | default AUR approval (→ `aur_approval`) |
