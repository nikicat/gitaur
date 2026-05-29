# Update loop, change-set preview & build-cost estimates (design)

Status: **phases 1–2 implemented; phases 3–4 pending.** The no-arg `gaur` loop,
session state, reviewed-set gating, picker badges, change-set preview, and
SIGINT-during-build → bail-to-table now live in `src/cli/upgrade_loop.rs` +
`src/build/makepkg.rs` (backed by `build::UpgradeSession` and the
`build::resolve_targets` / `build::apply_plan` split). Phase 2 added the
per-row size column + batch total on the change-set preview, reading from the
pacman DBs with no store (`PacmanIndex::{installed_size, sync_download_size}` +
`ui::human_bytes`). Phases 3–4 (build-time metric, custom picker) are not
started. The single-shot `-Syu` flow remains `dispatch::handle_s` in
`src/cli/dispatch.rs`.

## Problem

Today the upgrade flow is straight-line and one-shot
(`dispatch::handle_s`, `src/cli/dispatch.rs:50`):

```
refresh (-Sy) ──► collect_upgrade_plan ──► select_upgrades (picker)
              ──► run_repo_upgrade (pacman -Syu) ──► cmd_install (AUR) ──► exit
```

That exit is the problem. The real-world upgrade session is iterative:

1. The picker is dominated by repo packages on first open, so the natural
   first move is "apply the repo upgrades." After that, gitaur **exits.**
2. To continue with AUR packages you re-run `gaur`, which **re-fetches the
   mirror DBs** (`mirror::cmd_refresh`) before showing the picker again —
   tens of seconds of wasted work for a list that hasn't changed.
3. AUR builds fail (or get interrupted) more often than repo installs. When
   one fails you usually want to keep going with the others, then come back
   and retry — not restart the whole command.
4. Big AUR upgrades have wildly different costs. `cuda` is a multi-GiB
   *download*; `firefox-git` is a long *build*. Today the picker gives no
   signal, so you can't tell a 10-second upgrade from a 90-minute one before
   committing to it.
5. Selecting an AUR package silently drags in dependencies (repo deps, AUR
   deps, makedepends). The picker never shows that, so "I picked 1 package"
   can mean "I'm about to build 6."
6. Re-running after a failure re-prompts the PKGBUILD review for packages you
   already inspected and approved minutes ago.

## Decisions (from review)

These were settled before the design was finalized:

- **Loop only on no-arg `gaur`.** Explicit `gaur -Syu` keeps its current
  single-shot behavior (scriptable / yay-compatible). No new config knob.
- **Selecting a package expands its unsatisfied dependencies** into the
  table, visually indented, so the user sees the *total* change set.
  Implemented via **confirm-stage expansion** (route 1, below).
- **Ctrl+C during an AUR build bails to the table**, not out of gitaur. Only
  Ctrl+C (or a normal "done") *on the table* exits the program.
- **No in-table "refresh" action.** To pick up newer upstream versions the
  user exits and restarts — the fetch belongs at session start, not mid-loop.
- **Sizes are read from the pacman DBs, never stored.** Version, install
  date, and on-disk size of any installed package are already in pacman's
  localdb; sync-repo download sizes are in the sync DB. The picker only ever
  shows installed-but-outdated packages, so those rows always have a localdb
  entry to read. Manual-install edge cases (size drift between versions) are
  explicitly tolerated.
- **The only persisted metric is build time** — it cannot be derived from any
  pacman DB or on-disk artifact. Small SQLite store (`rusqlite`), keyed by
  pkgbase, introduced only when build-time tracking lands (a later phase). Also
  the right substrate for the eventual cross-user sharing feature.
- **Failed / interrupted rows stay in their natural sort position**, just
  badged — no shuffling to the top or bottom.
- **A session-only `reviewed` flag** records PKGBUILDs the user inspected and
  approved this session; reviewed packages are not prompted for review again.
  Not persisted to disk.
- **Phasing:** the loop is the core feature; cost metrics are auxiliary. Size
  comes first (free, from the DBs); build time follows (needs the store).

## Goals

- **One session, many batches** without re-fetching the mirror.
- **See the whole change set** — checking a package reveals the unsatisfied
  deps it pulls in.
- **Resilient to failure and interruption** — a failed or Ctrl+C'd AUR build
  drops you back to the table, never out of gitaur.
- **No redundant reviews** — approve a PKGBUILD once per session.
- **Cost visibility** — rows show a size estimate (later: build time).

## Non-goals (for this iteration)

- **Looping `-Syu`.** Explicit `-Syu` stays single-shot.
- **Cross-user sharing of timings.** Sketched at the end; out of scope here.
- **Changing non-interactive behavior.** `--noconfirm` / piped stdin / cron
  run one pass and exit (a loop needs a human).
- **Replacing the resolver / build pipeline.** The loop wraps the existing
  `resolver::resolve` / `cmd_install` / `run_aur_pipeline`.

## The session model

The loop hoists the expensive, once-per-session work *out* of the iteration
and keeps only the cheap recompute inside it.

```
                       ┌─ once per session (no-arg gaur) ───────┐
                       │  refresh mirror + index (-Sy)           │
                       │  load IndexFile + Secondary (mmap)      │   ← never repeated
                       │  open MirrorRepo                        │
                       └─────────────────────────────────────────┘
                                        │
            ┌──────────────── loop ─────▼─────────────────────────┐
            │  snapshot alpm localdb  (PacmanIndex::build, ~10ms)  │
            │  recompute remaining = repo upgrades ∪ AUR upgrades  │
            │  apply session state (skipped / failed / reviewed)   │
            │                          │                            │
            │            remaining empty? ──yes──► done             │
            │                          │no                          │
            │  picker:  (v = view PKGBUILD/diff → mark reviewed)    │
            │    check roots → resolve → confirm shows deps         │
            │    indented + change-set + cost total                │
            │                          │                            │
            │   empty selection / Ctrl+C on table ──yes──► exit    │
            │                          │no                          │
            │  apply batch:                                        │
            │    repo  → run_repo_upgrade (pacman -Syu --ignore)   │
            │    aur   → cmd_install(targets, already_confirmed)   │
            │            (review skipped for reviewed pkgbases)    │
            │  outcomes:                                           │
            │    success     → drops out next recompute            │
            │    failure     → session.failed[pkgbase] = reason    │
            │    Ctrl+C build→ mark interrupted, bail to table     │
            └──────────────────────────┘ back to top
```

### What stays out of the loop

`collect_upgrade_plan` (`src/build/upgrade.rs:29`) currently reloads the
index (`index::load_or_resync`) on every call. The loop must **not** do that
per iteration. The refactor: load `IndexFile` + `Secondary` + open the
`MirrorRepo` once at session start, then have a per-iteration
`recompute_remaining(&idx, &by, devel) -> Vec<PkgUpgrade>` that only
re-snapshots alpm. The index is immutable for the whole session (we already
fetched once); only the *localdb* changes as packages get installed.

### Why no re-fetch is correct

The candidate list shrinks naturally without touching the mirror:

- **Repo rows.** `query_repo_upgrades` (`src/pacman/invoke.rs:38`) compares
  localdb against gitaur's rootless syncdb. `pacman -Syu` moves localdb
  forward; the rootless syncdb is unchanged, so an upgraded repo pkg now
  vercmp-matches and drops out of the next recompute.
- **AUR rows.** `aur_upgrades` (`src/build/upgrade.rs:50`) compares localdb
  against the in-memory index. A successful `pacman -U` moves localdb
  forward; `Ver::is_outdated` goes false and the row drops out.
- **Failures stay.** A package that failed or was interrupted is still
  outdated, so it reappears next iteration — exactly what we want for retry.

A fresh fetch mid-session would only add brand-new upstream versions, not
worth a 10–30 s stall every iteration. Restarting `gaur` covers that case.
The mirror is fixed for the whole session, which also means a PKGBUILD's
content is fixed — so the `reviewed` flag can key on pkgbase alone.

### Interactive-only

The loop runs only for the no-arg invocation *and* only when interactive —
stdin is a TTY and `--noconfirm` is unset (the gate `select_upgrades` already
uses, `src/ui/tables.rs:188`). A non-interactive no-arg run does one pass and
exits, like `-Syu`.

## Change-set preview: expanding dependencies

The headline UX change. Today `select_upgrades` (`src/ui/tables.rs:177`) is a
flat checkbox list of upgrade candidates. Selecting an AUR package hides the
deps it drags in. We want the user to see the **unsatisfied** dependencies a
selection pulls in, indented, so the change set (and, later, its cost) is
honest.

### What "unsatisfied deps" means

Run the existing resolver on the confirmed selection and diff against
localdb. `resolver::resolve` (`src/resolver.rs`) already produces a `Plan`
with `direct_repo`, `transitive_repo`, and `aur_strata`. The rows surfaced
under a selected root are everything in that plan that is **not already
installed** and **not already shown** under another selected root:

- repo deps not installed → will be `pacman -S`'d (indented, repo-colored);
- AUR deps not installed → will be built (indented, the costly ones);
- makedepends / checkdepends not installed → build-time pulls (indented,
  dimmer).

A dep shared by two roots is shown once, attributed to the first root that
required it (the resolver's graph dedupes naturally).

### Rendering (route 1 — confirm-stage expansion)

`dialoguer::MultiSelect` can't expand rows inline on toggle — it's a flat
list with no toggle-time hook, and its redraw math is already delicate (see
the `UpgradePickerTheme` ANSI workaround, `src/ui/tables.rs:141`). So:

- Keep the flat MultiSelect for picking **roots**.
- After the user confirms, resolve the selection and render the expanded
  change-set table at the existing confirm gate, before any sudo/build:

```
this batch — 3 packages, +2 deps:
  aur   cuda            12.6-1   ->  12.8-1     ~3.0 GiB
    └ extra  gcc13          (install)            128 MiB
    └ aur    nvidia-utils   (build)             ~280 MiB
  aur   yay-bin         12.4-1   ->  12.5-1       ~9 MiB
  core  glibc           2.40-1   ->  2.41-1       12 MiB
                                          total · ~3.4 GiB   continue? [Y/n]
```

The "table extended with indented deps" lives on this confirm screen rather
than in the live picker. A live inline-expanding custom picker (route 2) is
the eventual target but is deferred — both routes render the identical
change-set computation; only *when* it appears differs.

**As shipped (phases 1–2)** — `ui::change_set_table`, driven by
`upgrade_loop::preview`. One simplification remains against the sketch above:

- **Sizes (phase 2).** Every row carries a right-aligned size cell and the
  table closes with a batch total. Repo rows/deps show the exact download size
  from the syncdb (`PacmanIndex::sync_download_size`, cached pkgs read 0); AUR
  roots show a `~`-prefixed estimate from the installed footprint
  (`PacmanIndex::installed_size`, via localdb `isize`); never-installed
  pulled-in AUR builds show `~?`. The total is `~`-prefixed whenever any row is
  an estimate or unknown — see `ui::human_bytes` and the `SizeEst` classifier
  in `src/ui/tables.rs`.
- **Flat deps, not per-root nesting.** The pulled-in deps render as one
  indented block under a `pulls in:` line, not nested beneath the specific
  root that dragged each in. The resolver's `Plan` tracks AUR build edges
  (`aur_make_edges`) but not per-root provenance for *repo* deps, so honest
  per-root attribution isn't a cheap read today. Deferred to route 2 (phase
  4), where the live picker rebuilds the change-set model anyway.

So the confirm screen currently looks like:

```
:: this batch — 3 package(s), +2 dependencies
    core  glibc      2.40-1 -> 2.41-1   12.00 MiB
    aur   cuda       12.6-1 -> 12.8-1   ~3.00 GiB
    aur   yay-bin    12.4-1 -> 12.5-1   ~9.00 MiB
-> pulls in:
      gcc13          (install)   50.00 MiB
      nvidia-utils   (build)            ~?
-> total  ~3.06 GiB
Proceed with this batch? [Y/n]
```

## PKGBUILD review across the session

Review already happens today: phase 1 of `run_aur_pipeline` calls
`review::review` per pkgbase before any build (`prepare_one`,
`src/build.rs:537`), returning `Approved` / `Skipped` per `review_default`
(`prompt` / `skip` / `always-show`). The session adds a `reviewed` set so the
user approves a given PKGBUILD at most once per session:

- `session.reviewed: HashSet<PkgBase>` — session-only, never written to disk.
- Before prompting, `prepare_one` checks the set; a hit is auto-`Approved`
  with no prompt. A genuine review that ends in approval inserts the pkgbase.
- Within a session the mirror is fixed (no re-fetch), so a pkgbase maps to one
  PKGBUILD commit — keying on pkgbase alone is sound; no need to key on commit.
- This is what makes retry-after-failure painless: a `firefox-git` you
  approved in iteration 1 and that then failed is re-offered for build in
  iteration 2 **without** re-prompting the diff.

### View hotkey from the table

The user can also review proactively: a `v` hotkey on the highlighted row
opens its PKGBUILD (fresh install) or diff-against-installed (upgrade) — the
same `review` rendering — and, on approval, adds the pkgbase to
`session.reviewed` so the later build won't prompt again.

`dialoguer::MultiSelect` has no custom-key hook, so the `v` hotkey requires
the route-2 custom picker. It therefore lands with route 2 (phase 4). Until
then, review happens at the existing pre-build step and the `reviewed` set
still does the dedup across iterations — the hotkey is an ergonomics add, not
a correctness requirement. (Two features now want the custom picker — live
dep-expansion and the `v` hotkey — which may argue for pulling route 2
forward; noted, but phase 1 does not depend on it.)

## Signals & interruption

| Context when Ctrl+C arrives | Result |
| --------------------------- | ------ |
| An AUR build (`makepkg`) is running | abort *that* build, mark the pkgbase interrupted, **return to the table** |
| The picker/table is displayed | **exit gitaur** |
| Normal "done" (empty selection) on the table | exit gitaur |

Mechanism (the trickiest part of phase 1):

- makepkg runs under a pty (`src/build/makepkg.rs`). The build phase installs
  a scoped SIGINT handler (or a watched `AtomicBool` + forward) so an
  interrupt during a build forwards the signal to the makepkg child, waits
  for it to die, marks the pkgbase interrupted in `RunReport`, and unwinds via
  a dedicated `Error::Interrupted` that `run_aur_pipeline` / `cmd_install`
  propagate up to the loop **as "batch aborted," not "exit."**
- The loop catches `Error::Interrupted`, folds the partial `RunReport` into
  session state, and re-enters the table. Anything already built+installed in
  the batch stays (localdb updated); the interrupted pkgbase and anything
  after it remain outstanding.
- While the table is showing, the handler is not installed (or is default),
  so Ctrl+C / dialoguer's abort path exits cleanly. Today an empty selection
  returns `Error::UserAbort` (`src/cli/dispatch.rs:100`); in loop mode an
  empty selection means "done" → `Ok(0)`, and `UserAbort` is reserved for the
  sudo-gate decline inside a batch.

Worth prototyping early — getting signal delivery right across the pty
boundary (child gets the signal to stop; gitaur intercepts to survive) is
fiddly and underpins the whole "resilient session" promise.

## Session state vs persisted state

| State | Lifetime | Storage | Used for |
| ----- | -------- | ------- | -------- |
| `skipped: HashSet<PkgBase>` | one session | in memory | de-prioritize / pre-uncheck passed-on rows |
| `failed: HashMap<PkgBase, String>` | one session | in memory (from `RunReport`) | badge + reason; pre-uncheck |
| `interrupted: HashSet<PkgBase>` | one session | in memory | distinct badge from failed |
| `reviewed: HashSet<PkgBase>` | one session | in memory | suppress repeat PKGBUILD review |
| **build time** | across sessions | **SQLite store** | build-time column + batch total (later phase) |

`RunReport` (`src/build.rs:413`) already carries `installed` / `failed` /
`skipped_user` / `skipped_dep` per batch — the loop folds it into the session
sets after each `cmd_install` (and on `Error::Interrupted`).

## Cost estimates

### Size — from the pacman DBs, no store — DONE

For any candidate row gitaur reads size without persisting anything, because
the picker only shows installed-but-outdated packages:

- **Repo rows / repo deps** — exact compressed download size from the sync DB
  (`alpm::Package::download_size()` on the syncdb pkg).
- **AUR rows** — the currently-installed version's on-disk size from localdb
  (`alpm::Package::isize()`). This is the uncompressed installed size of the
  *old* version, used as a predictor of the new build's footprint — good enough
  to rank "small vs huge," which is the actual need. Marked with a leading `~`.
- **AUR deps** — the pulled-in build deps are by definition not yet installed
  (the resolver only surfaces *unsatisfied* deps), so they have no localdb size
  and render `~?`.

`PacmanIndex` (`src/pacman/alpm_db.rs`) gained two pkgname-keyed maps for this —
`installed_size` (localdb `isize`) and `sync_download_size` (syncdb
`download_size`) — built in the same single pass, so the picker reads sizes
without holding `&Alpm`. No SQLite, no new files. (The sketch floated also
carrying `base()` to sum a split AUR pkgbase's installed footprint across its
members; the single-pkgname `isize` is enough for the small-vs-huge ranking, so
that wasn't added.)

Imprecision is accepted by decision: installed≠download size, and a manually
installed package may differ from what gitaur would build. The number is a
hint, not a contract.

### Build time — the one stored metric (later phase)

Build duration is the only cost that cannot be read back from any pacman DB or
artifact. When build-time tracking lands:

- Store `state_dir()/metrics.db` (new `paths::metrics_db_path()`, `rusqlite`):

  ```sql
  CREATE TABLE IF NOT EXISTS build_metrics (
      pkgbase    TEXT PRIMARY KEY,
      build_secs INTEGER NOT NULL
  );
  ```

  That's the whole schema. The version/date context for staleness is *also*
  deducible from localdb (the install date ≈ when it was last built), so it
  isn't duplicated here. A `source_dl_bytes` column can be added later if the
  source-download figure (cuda's cold-cache GiB) proves worth measuring
  separately from installed size.
- Capture by wrapping the `makepkg::run` call in `run_build`
  (`src/build.rs:560`) with an `Instant`; upsert on success. The `Cached`
  disposition (`src/build.rs:515`) skips the build and leaves the row intact.

### Batch total before apply — DONE

The confirm screen sums the size column of the whole resolved change set
(roots + unsatisfied deps; exact for repo, estimated for AUR) into the
`total  ~X` line shown above — `~`-prefixed when any row is an estimate or
unknown (`tables::batch_total`). Once build time exists, a `~Yh build` term can
join it. Cheap — a sum at a gate that already exists.

## Ordering with session state

Keep the existing repo-group/severity sort (`sort_for_display`,
`src/ui/tables.rs:259`) unchanged — rows stay where the user expects them. The
only addition is a per-row badge and default-check tweak:

- Failed this session → unchecked, red `(failed)` badge, in place.
- Interrupted this session → unchecked, `(interrupted)` badge, in place.
- Skipped this session → unchecked, dim, in place.
- Reviewed this session → small `✓` marker so the user knows it won't prompt.

No row moves; the badge carries the state.

## Suggested phasing

1. **Loop core. — DONE.** Hoist index/mirror load out of the iteration; add
   `recompute_remaining`; turn the no-arg upgrade path into a loop with clean
   empty-selection exit; fold `RunReport` into session state; add the
   `reviewed` set and gate `prepare_one`'s review on it; implement the
   SIGINT-during-build → bail-to-table behavior (`Error::Interrupted`);
   change-set preview via confirm-stage expansion (route 1). No cost numbers
   yet. Removes the re-fetch pain, the failure/interrupt restart pain, the
   redundant-review pain, and the hidden-deps problem. Landed in
   `src/cli/upgrade_loop.rs` (`drive`/`RealEnv` over the `LoopEnv` seam) +
   `src/build/makepkg.rs` (signal handling). Tests: loop logic via the
   `LoopEnv` fake (`upgrade_loop` unit tests), SIGINT and the loop UI via
   `tests/container/extended/02`/`04`. The change-set preview shipped flat (no
   per-root nesting) — see "As shipped" above.
2. **Size column + batch total. — DONE.** Renders a size on every change-set
   row (repo exact from syncdb `download_size` / AUR estimated from localdb
   `isize`, `~?` for never-installed pull-ins) and the post-selection total. No
   store — pure DB reads via the two new `PacmanIndex` size maps. Landed in
   `src/ui/tables.rs` (`SizeEst`, `change_set_table`) + `src/ui.rs`
   (`human_bytes`) + `src/pacman/alpm_db.rs` (size maps). The `base()` summing
   the sketch floated was dropped as unnecessary — see "Size" above.
3. **Build-time metric.** Add `rusqlite` + `metrics.db` (just
   `pkgbase → build_secs`); capture around `makepkg::run`; add the build-time
   term to the column and total; dim estimates whose localdb install date is
   old.
4. **Polish / custom picker (route 2).** Live inline dep-expansion and the
   `v` review hotkey in a custom picker; optional `source_dl_bytes`.

Phase 1 is independently shippable and delivers most of the felt
improvement; 2–3 are the cost-visibility payload; 4 is the slicker picker.

## Cross-user sharing (future, out of scope)

The eventual goal is shared timings ("firefox-git is ~80 min" learned once,
by everyone). The SQLite choice eases this. Notes:

- **Build time is the thing worth sharing** — it's exactly what no local DB
  can supply for a not-yet-built package. But it does not travel without
  normalization (CPU, cores, ccache, load); a shared figure needs a
  host-class normalizer and outlier rejection.
- **Size needs no sharing** — it's already a local DB read.
- **Trust.** Any user-contributed, package-keyed data is an abuse vector;
  treat shared numbers as hints, never as gates on what gets built.
