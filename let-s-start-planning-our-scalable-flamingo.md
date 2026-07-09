# aurox — yay-like AUR helper backed by the GitHub mirror

> **Historical planning document.** This is the original master plan /
> brainstorm that kicked off aurox. It is kept for context only — the
> "Status (updated)" section near the bottom is a stale snapshot (it still
> mentions a `build/state_db.rs`, a never-completed bootstrap clone, "43
> tests", and features long since exercised end-to-end). For the current,
> maintained picture see:
> [`README.md`](README.md) (usage),
> [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (internals),
> [`docs/TESTING.md`](docs/TESTING.md),
> [`docs/COMPARISON.md`](docs/COMPARISON.md),
> [`docs/UPDATE_LOOP.md`](docs/UPDATE_LOOP.md), and
> [`docs/FETCH_OPTIMIZATION.md`](docs/FETCH_OPTIMIZATION.md).
> Don't trust the status claims below; trust the code and `docs/`.

## Context

`aur.archlinux.org` has poor uptime; the user prefers the github.com/archlinux/aur monorepo (one branch per package, ~155k branches, ~2 GiB pack) as the source of truth. No mainstream AUR helper uses it. We benchmarked:

- Full `.SRCINFO` scan of all 155k branches via `git2`: **~2.0 s with 4 threads** (4.0 s single-threaded). Phase B (ref → commit → tree → blob OID) dominates at 2.4 s; blob decompression is secondary at 1.4 s.
- Disabling pack deltas / zlib does **not** help — graph traversal cost grows with pack size; default packing is near-optimal.
- `git fetch --porcelain` reports changed refs cheaply, enabling incremental re-index of just the deltas.

These numbers say: a libgit2-native, rayon-parallel helper with a pre-parsed in-memory index can give yay-class UX while being fully offline-from-AUR. That's `aurox`.

User decisions already made: full pacman pass-through, recursive AUR dep resolution, `git worktree` staging, full-PKGBUILD-on-first-install + diff-on-update.

## Project layout

```
~/src/aurox/
├── Cargo.toml
├── README.md (later)
├── src/
│   ├── main.rs                # argv → cli::run
│   ├── error.rs               # crate Error/Result via thiserror
│   ├── log.rs                 # color-aware print helpers
│   ├── paths.rs               # state_dir(), config_dir(), aur_repo_path(), pkg_worktree(name)
│   ├── config/
│   │   ├── mod.rs             # Config, load()
│   │   └── defaults.rs        # built-in defaults
│   ├── cli/
│   │   ├── mod.rs             # Cli (clap derive), Command enum
│   │   ├── dispatch.rs        # decides own-vs-passthrough
│   │   └── flags.rs           # pacman-style cluster parsing
│   ├── mirror/
│   │   ├── mod.rs             # MirrorRepo wrapper; thread_local() factory
│   │   ├── clone.rs           # bootstrap_clone(state_dir)
│   │   ├── fetch.rs           # incremental_fetch() → Vec<RefUpdate> via update_tips cb
│   │   └── worktree.rs        # add_worktree / prune_worktree
│   ├── index/
│   │   ├── mod.rs             # public Index API (load/save/query)
│   │   ├── schema.rs          # rkyv-archived IndexEntry, IndexFile
│   │   ├── srcinfo.rs         # line-oriented .SRCINFO parser
│   │   ├── build.rs           # full_build(repo) — rayon-parallel
│   │   ├── update.rs          # incremental_update(repo, &[RefUpdate])
│   │   └── secondary.rs       # by_name / by_provides / by_depends_on hashmaps
│   ├── resolver/
│   │   ├── mod.rs             # DepGraph, resolve(targets) → Plan
│   │   ├── classify.rs        # Source::{Installed,Repo,Aur,Missing} via alpm + index
│   │   └── topo.rs            # Tarjan topo-sort + cycle reporting
│   ├── build/
│   │   ├── mod.rs             # Builder, build_one(pkgbase)
│   │   ├── review.rs          # show_pkgbuild_or_diff(pkgbase, last_oid)
│   │   ├── makepkg.rs         # spawn makepkg, stream stdio
│   │   ├── install.rs         # pacman -U with sudo
│   │   └── state_db.rs        # rusqlite store: last_built_commit_oid per pkgbase
│   └── pacman/
│       ├── mod.rs
│       ├── alpm_db.rs         # read-only alpm handle, installed_version, providers
│       ├── vercmp.rs          # alpm_pkg_vercmp wrapper
│       └── invoke.rs          # exec_pacman(args), sudo gating
└── tests/
    ├── srcinfo_parser.rs      # goldens in tests/fixtures/srcinfo/
    └── fake_mirror.rs         # tiny local bare repo with 5–10 fixture branches
```

State at runtime:

```
~/.local/state/aurox/
├── aur/              # bare git2 clone of github.com/archlinux/aur (~2 GiB)
├── pkgs/<pkgbase>/   # one worktree per package being built (kept until --clean)
├── index.bin         # rkyv-archived IndexFile, mmap'd at load (~60–80 MB)
└── state.db          # SQLite: builds(pkgbase, last_built_commit_oid, last_built_version, built_at)

~/.config/aurox/config.toml  # optional, see §6
```

## Design (recommended approach)

### 1. Index format

**rkyv 0.8 zero-copy archive, mmap-loaded.** Open file → mmap → validate header → ready. No deserialization pass, no heap allocs per entry. Beats bincode (~50–150 ms parse + Vec<String> allocs) and SQLite (overkill for a read-mostly scan-friendly path).

Schema (`src/index/schema.rs`):

```rust
#[derive(Archive, Serialize, Deserialize)]
pub struct IndexEntry {
    pub pkgbase: String,
    pub pkgnames: Vec<String>,                  // split-package members
    pub pkgver: String, pub pkgrel: String, pub epoch: Option<String>,
    pub pkgdesc: Option<String>,
    pub depends: Vec<String>, pub makedepends: Vec<String>,
    pub checkdepends: Vec<String>, pub optdepends: Vec<String>,
    pub provides: Vec<String>, pub conflicts: Vec<String>, pub replaces: Vec<String>,
    pub arch: Vec<String>,
    pub commit_oid: [u8; 20], pub srcinfo_blob_oid: [u8; 20],
}

#[derive(Archive, Serialize, Deserialize)]
pub struct IndexFile {
    pub format_version: u32,
    pub mirror_head_oid: [u8; 20],
    pub built_at_unix: u64,
    pub entries: Vec<IndexEntry>,               // sorted by pkgbase
}
```

Secondary indexes (built post-load, ~50 ms, in `src/index/secondary.rs`):

- `by_name: HashMap<String, u32>` (pkgname → entries idx; split pkgs map multiple names to same idx)
- `by_provides: HashMap<String, SmallVec<[u32; 2]>>`
- `by_depends_on: HashMap<String, SmallVec<[u32; 4]>>` — built lazily, only when `-Syu` needs it

Search (`-Ss`) walks `entries` linearly with rayon (~50 ms for 155k regex matches).

### 2. Sync / fetch flow

`aurox` (no args) and `aurox -Sy`:

1. `mirror::open_or_bootstrap()`:
   - If `~/.local/state/aurox/aur/` missing: full bare clone from `https://github.com/archlinux/aur.git` via `git2::build::RepoBuilder::new().bare(true).clone(...)`, with `RemoteCallbacks::transfer_progress` reporting bytes + objects to the terminal.
   - Else: open the existing bare repo with `Repository::open_bare`.
2. `incremental_fetch()`: `remote.fetch(&["+refs/heads/*:refs/heads/*"], opts, None)` with an `update_tips` callback that collects `(refname, old_oid, new_oid)` into a `Vec<RefUpdate>`. First fetch after bootstrap skips this since the index was freshly built.
3. If updates exist: `index::update::incremental_update(&repo, &updates, &mut index)` — for each changed ref, resolve `new_oid → tree → .SRCINFO blob`, reparse, replace entry. Deletions → `new_oid == zero` → drop entry.
4. **Atomic swap**: write `index.bin.tmp`, `rename(2)` over `index.bin`.
5. **Fetch failure**: warn, leave on-disk index untouched, continue with stale data. Don't block `-S` of already-known packages.

First run: ~5 min clone + ~4 s full index. Subsequent: typically 100–500 ms fetch + <100 ms reindex.

### 3. Install flow (`-S pkg1 pkg2 …`)

#### Phase A — planning (no side effects yet)

1. Refresh mirror + index unless `--no-refresh`.
2. Resolve each user target via `index.by_name`, falling back to `by_provides`. Map pkgname → pkgbase via the index entry. Unresolved → abort listing all unknown targets.
3. Build dep graph in `resolver::DepGraph`:
   - BFS over `depends` + `makedepends` + `checkdepends` for each AUR node (only `depends` for already-installed leaves).
   - Strip version constraint operators (`>=`, `=`, `<`, etc.) per dep string before lookup.
   - Classify each dep via `resolver::classify`:
     - `pacman::alpm_db::installed_version(name).is_some()` → `Installed`.
     - alpm syncdb hit (`pacman::alpm_db::syncdb_provides(name)`) → `Repo`.
     - `index.by_name.get(name)` or `index.by_provides.get(name)` → `Aur(pkgbase)`. Recurse.
     - Otherwise → `Missing`.
   - All `Missing` collected first; if non-empty, abort with the full list (don't trickle errors).
4. `resolver::topo::sort()` produces a deterministic order. Cycles printed as readable paths (`a → b → c → a`) and abort.
5. Compute `Plan { repo_deps: Vec<String>, aur_order: Vec<String>, direct_targets: HashSet<String> }`. Print grouped summary (`Repo (N): …`, `AUR (M): …`) and prompt `Proceed? [Y/n]` via `dialoguer`. Default is yes; `--noconfirm` skips.

#### Phase B — sudo warmup

6. Before any destructive operation, run `sudo -v` (or the configured `privilege_escalator`) to seed the sudo timestamp, and spawn a background keepalive task that runs `sudo -n -v` every 60 s for the duration of the install. Avoids password prompts mid-build. Killed in a `Drop` on the warmup guard.

#### Phase C — repo deps batched into one pacman call

7. If `!repo_deps.is_empty()`: `exec_pacman(["-S", "--needed", "--asdeps", ...repo_deps])`. Direct targets that happen to be in repos are still installed via this call but without `--asdeps`; we split into two pacman invocations only if both sets are non-empty.

#### Phase D — per-pkgbase build loop (sequential)

For each `pkgbase` in `aur_order`:

8. **Worktree creation** (`mirror::worktree::add_worktree`):
   - Target path: `~/.local/state/aurox/pkgs/<pkgbase>`.
   - If path exists from a prior run: open it with `git2::Repository::open`, check that `HEAD` is on `refs/heads/<pkgbase>`, then `repo.reset(&new_head, git2::ResetType::Hard, None)` to the current mirror tip — preserves the worktree for diffs and avoids `git2::Worktree::prune` churn. If path exists but isn't a valid worktree (stale / corrupted), delete and recreate.
   - Fresh worktree: `mirror.worktree("<pkgbase>", path, Some(&WorktreeAddOptions::new().reference(<branch>)))`. Branch reference is `refs/heads/<pkgbase>` resolved on the bare mirror repo.

9. **PKGBUILD review** (`build::review::show_pkgbuild_or_diff`):
   - Look up `state.db.builds(pkgbase)`:
     - **First install** (row absent): cat the worktree's `PKGBUILD` then its `.SRCINFO` to stdout, with line wrapping and a header banner. Pipe through `$PAGER` if stdout is a TTY and the content > one screen (use `console::Term::size_checked` to decide).
     - **Update** (row present): compute `git2::Diff::tree_to_tree(repo, &state.last_built_tree, &head_tree, None)`, format as `DiffFormat::Patch`, colorize via `console::style` (green/red for +/-), then page if long. If the diff is empty (rebuild requested without upstream change), say so explicitly.
   - Prompt loop via `dialoguer::Select` or a custom keystroke read:
     - `[Enter]` (default) — proceed to build
     - `[v]iew` — re-print PKGBUILD/diff (useful after edit)
     - `[e]dit` — spawn `$EDITOR` (fallback `vi`) on `<worktree>/PKGBUILD`; after exit return to prompt with re-shown view
     - `[d]iff` — for first-install case only, show the full PKGBUILD again; for update case toggle full PKGBUILD vs diff
     - `[s]kip` — drop this pkgbase from the plan, continue with the rest (records nothing in `state.db`)
     - `[a]bort` — exit nonzero; previously-installed deps remain installed
   - `--noconfirm` short-circuits the prompt (auto-proceed) but still prints a one-line "building <pkgbase>" header.

10. **Pre-build cleanup**:
    - Remove any `*.pkg.tar.zst` files left in the worktree from a prior failed run (we'll re-detect produced files post-build by mtime, but a clean slate avoids ambiguity).
    - `unlink` the worktree's `pkg/` and `src/` dirs if they exist and the user opted into `--clean-build` (default off; makepkg's `-C` flag handles this when on).

11. **makepkg invocation** (`build::makepkg::run`):
    - Cwd = `<worktree>`. Env inherited plus:
      - `PKGDEST=<worktree>` (built `.pkg.tar.zst` lands in the worktree dir — predictable glob target).
      - `SRCDEST=<worktree>/src-cache` (source tarball cache, survives across rebuilds).
      - `BUILDDIR=<worktree>` (compile scratch).
      - `MAKEFLAGS` from config (defaults to `-j$(nproc)`).
    - Args: from `config.makepkg_args` (default `["-s", "--noconfirm", "--needed"]`).
      - `-s` → install build-time deps via pacman. Redundant with our Phase C batch but acts as a belt-and-suspenders check; the cost is one alpm read.
      - `--noconfirm` → no interactive prompts from makepkg itself.
      - `--needed` → don't reinstall already-current packages.
      - We do **not** pass `-i` (makepkg's auto-install) — aurox installs from the built file with explicit `--asdeps` semantics in step 13.
    - Spawn via `std::process::Command`, inherit stdio so user sees compile output live. Capture exit code; on nonzero, jump to failure handling below.

12. **Detect produced packages**:
    - Glob `<worktree>/*.pkg.tar.zst` (and `*.pkg.tar.xz` for older configs; honor `PKGEXT` if set).
    - For split packages, multiple files will match — one per pkgname in the SRCINFO `pkgname = …` lines. Validate that the set of detected files covers all pkgnames listed in the index entry; missing → warn but proceed with what we have.

13. **Install with pacman**:
    - Partition produced files: those whose pkgname is in `direct_targets` → install without `--asdeps`; the rest → install with `--asdeps` (so they're marked as dependencies and orphan detection works later).
    - If both sets are non-empty, two pacman invocations: `exec_pacman(["-U", "--needed", "--noconfirm", ...direct_files])` then `exec_pacman(["-U", "--needed", "--asdeps", "--noconfirm", ...transitive_files])`. Otherwise one.

14. **Record success** in `state.db.builds`: upsert `(pkgbase, last_built_commit_oid = HEAD oid, last_built_version = "<epoch>:<pkgver>-<pkgrel>", built_at = now())`. Write in a single `INSERT … ON CONFLICT(pkgbase) DO UPDATE` statement.

15. **Worktree retention**: kept under `pkgs/<pkgbase>/` so the next update can diff against it via `state.last_built_tree`. The `state_db` row is the source of truth for "last built revision"; the worktree is convenience. `aurox -Sc` clears worktrees but preserves `state.db`; `aurox -Scc` clears both.

#### Failure handling within Phase D

- **Build (makepkg) failure**: log the pkgbase and exit code, leave the worktree alone (user can inspect logs, the `src/` and `pkg/` dirs), do NOT update `state.db`. Continue to next pkgbase only if `--keep-going`; default is to stop the chain.
- **Install (pacman -U) failure**: same — leave the `.pkg.tar.zst` in place so user can `pacman -U` manually; don't touch `state.db`.
- **User abort during review**: clean exit; deps installed earlier remain (they're useful regardless). Rerun continues from the abort point (`--needed` in Phase C is idempotent; Phase D iterates remaining targets).
- **Mid-chain stop**: `aur_order` is deterministic, so a rerun of the same `-S` command picks up where the previous one stopped — already-installed-or-built pkgbases are detected by alpm + version compare and skipped.

#### Split packages

A single `pkgbase` PKGBUILD can produce N pkgnames. The dep graph treats pkgbase as the node. Step 11 builds once; step 12 finds N `.pkg.tar.zst`; step 13 partitions by which pkgnames the user actually asked for (direct vs transitive). `state.db` records one row per pkgbase regardless of pkgname count.

### 4. Upgrade flow (`-Syu`)

Order: **pacman first, then AUR** (AUR builds may link against freshly-upgraded repo libs).

1. `aurox -Sy` (refresh mirror + index).
2. `exec_pacman(["-Syu", ...passthrough_args])`.
3. AUR upgrade detection:
   - `alpm.localdb().pkgs()` filtered to those NOT in any syncdb → foreign candidates.
   - For each, `index.by_name` lookup; missing → log "foreign, not in AUR" and skip.
   - `alpm_pkg_vercmp(installed, index_version)` > 0 → queue.
   - VCS pkgs (`-git`/`-svn`/`-hg`/`-bzr`, or `pkgver` starting `r\d+\.`): skip by default; `--devel` opts in and queues unconditionally (makepkg's `pkgver()` regenerates).
4. Run install flow §3.6–8 on the queue.

### 5. PKGBUILD review state

SQLite at `~/.local/state/aurox/state.db` (single table, see schema above) via `rusqlite` (bundled feature so no system-libsqlite3 dependency). Row-level atomicity, future-proof for concurrent invocations. Diff computation uses `git2::Diff::tree_to_tree`.

### 6. Threading model

- Index full build: rayon pool of 4 threads (sweet spot from benchmarks); each worker opens its own `git2::Repository` via `MirrorRepo::thread_local()` (validated thread-safety pattern). Branches partitioned, results concatenated + sorted in main.
- Incremental update: same pattern, par_iter over changed refs.
- `-Ss` regex match: `entries.par_iter().filter(...)`.
- alpm vercmp during `-Syu`: serial (alpm handle is single-threaded; cost is negligible).
- Build phase: strictly sequential per pkgbase (makepkg owns MAKEFLAGS for inner parallelism; cross-pkgbase parallel builds rejected due to shared pacman state).

### 7. Pacman pass-through (`cli::dispatch`)

clap with `allow_external_subcommands(true)`. Owned flag-combinations:

- `-S`, `-Sy`, `-Syu`, `-Syyu` (+ `--asdeps`, `--needed`, `--noconfirm`, `--devel`)
- `-Ss` (search merged: pacman repos then AUR)
- `-Si` (info; AUR fallback if not in repos)
- `-Sc` / `-Scc` (also cleans `pkgs/` worktrees + orphan `state.db` rows)

Everything else (`-Q`, `-R`, `-T`, `-D`, `-F`, `-U`-direct, `-Sg`, etc.) → `exec_pacman(argv)`. Sudo gate: prepend `sudo` (configurable: `doas`, `run0`) when op ∈ `{-S, -R, -U, -Sy, -Syu, -Syyu, -Sc, -D}` and not `--print`/`-p`.

### 8. Config (`~/.config/aurox/config.toml`)

```toml
build_dir = "~/.local/state/aurox/pkgs"
mirror_url = "https://github.com/archlinux/aur.git"
index_threads = 4
refresh_max_age_secs = 3600        # `aurox` no-args refetches if older
color = "auto"                     # auto | always | never
makepkg_path = "makepkg"
makepkg_args = ["-s", "--noconfirm", "--needed"]
privilege_escalator = "sudo"       # sudo | doas | run0
devel = false                      # include -git/-svn in -Syu
review_default = "prompt"          # prompt | skip | always-show
```

Loaded once in `main`, shared as `Arc<Config>`.

### 9. Crate dependencies

```toml
[dependencies]
git2 = "0.20"                                                # libgit2 — bare clone, worktrees, fetch, diff
clap = { version = "4", features = ["derive"] }              # CLI
rayon = "1.10"                                               # parallelism
alpm = "4"                                                   # read pacman DB
alpm-utils = "4"                                             # target/vercmp helpers
rkyv = { version = "0.8", features = ["validation"] }        # index file
memmap2 = "0.9"                                              # mmap index.bin
rusqlite = { version = "0.32", features = ["bundled"] }      # state.db
serde = { version = "1", features = ["derive"] }             # config
toml = "0.8"                                                 # config parse
dialoguer = "0.11"                                           # review prompts
console = "0.15"                                             # color, paging
anyhow = "1"                                                 # bin errors
thiserror = "1"                                              # lib errors
dirs = "5"                                                   # XDG paths
regex = "1"                                                  # -Ss
smallvec = "1"                                               # secondary-index values
nix = { version = "0.29", features = ["unistd"] }            # geteuid
log = "0.4"
env_logger = "0.11"
```

Skipped: tokio (no async need), reqwest (git2 handles network), tracing (env_logger suffices early).

## Scope (single milestone — full implementation)

Ship the complete tool in one pass. The whole feature set above is in scope:

- Bootstrap clone of the mirror; incremental fetch with porcelain-driven re-index.
- Rkyv index with secondary hashmaps, rayon-parallel build at 4 threads, atomic swap.
- Full CLI: `-S`, `-Sy`, `-Syu`, `-Syyu`, `-Ss`, `-Si`, `-Sc`, `-Scc`; everything else passes through to pacman.
- Recursive AUR dep resolution with topo-sort and cycle reporting.
- alpm-driven installed/repo classification; batched repo-dep install in one pacman call.
- Full build flow as specified in §3 — sudo warmup, worktree-per-pkgbase, PKGBUILD review (full on first install / diff on update) with `[V]/[E]/[D]/[S]/[A]/Enter` prompt loop, makepkg invocation with PKGDEST/SRCDEST/BUILDDIR env, split-package partitioning, state.db recording.
- `-Syu` AUR upgrade detection: foreign-package vs index version compare; `--devel` opts in VCS packages.
- TOML config at `~/.config/aurox/config.toml` with documented defaults.

Build order within the single milestone (each step independently runnable, total ~2 weeks):

1. `paths`, `error`, `log`, `config` (defaults + TOML loader).
2. `mirror::clone` + `mirror::fetch` against a small fake bare repo first, then against the real github mirror.
3. `index::srcinfo` parser with goldens; `index::schema` (rkyv); `index::build` single-threaded, then parallelize with rayon.
4. `index::update::incremental_update` driven by `update_tips`; atomic file swap.
5. `cli::dispatch` + `pacman::invoke` passthrough — wire `-Q`, `-R`, etc. through to pacman.
6. `pacman::alpm_db` + `pacman::vercmp`; `-Ss` and `-Si`.
7. `resolver::classify` + `resolver::topo` + `resolver::DepGraph` against fake-mirror fixtures.
8. `build::makepkg` + `build::install` with hardcoded "no deps" path against `cower` fixture.
9. `mirror::worktree` add/reset semantics; full Phase D build loop.
10. `build::review` + `build::state_db`; first-install vs diff-on-update paths.
11. `-Syu` AUR upgrade detection and chained invocation of the build loop; `--devel` handling.
12. `-Sc` / `-Scc` cleanup; cycle detection error UX; final polish.

## Verification

End-to-end smoke fixtures (mostly small, fast, low-risk):

- `cower` — simple, 1 makedep, no runtime deps. Happy path.
- `pkgstats` — depends only on `python` (repo). Verifies repo-only batching.
- `paru-bin` — `provides=('paru')`; tests provides-lookup and `-bin` semantics.
- `yay` — `go` makedep; small recursive chain.
- `downgrade` — depends on `pacman-contrib` (repo); AUR→repo edge.
- `mingw-w64-gcc` — pkgbase produces 5 split pkgs. Slow; run only when testing splits.
- `brave-bin` — frequent version bumps; good `-Syu` exercise across days.
- `neovim-git` — VCS pkg with `pkgver()`; `--devel` test.

Test harness:

- `tests/fake_mirror.rs` — local bare repo with hand-crafted branches, no network. Drives index build, fetch-detection, dep resolution offline.
- `tests/srcinfo_parser.rs` — golden files in `tests/fixtures/srcinfo/` (~20 real `.SRCINFO`s, including split + arch-specific + VCS examples).
- `criterion` benches: `bench_index_build` (target ≤2.2 s), `bench_index_load` (target ≤500 ms incl. secondary indexes).
- **Sandbox dogfooding**: real `aurox -S <fixture>` runs inside `systemd-nspawn` Arch container so host stays clean. Script kept at `scripts/smoke.sh` once we have something to test (not created up front).
- Pre-v0.3 release gate: 2 weeks of `aurox -Syu` on the dev box alongside yay, aurox first, log mismatches.

## Critical files to be created

- /home/nb/src/aurox/Cargo.toml
- /home/nb/src/aurox/src/main.rs
- /home/nb/src/aurox/src/paths.rs
- /home/nb/src/aurox/src/mirror/clone.rs
- /home/nb/src/aurox/src/mirror/fetch.rs
- /home/nb/src/aurox/src/index/srcinfo.rs
- /home/nb/src/aurox/src/index/schema.rs
- /home/nb/src/aurox/src/index/build.rs
- /home/nb/src/aurox/src/cli/mod.rs
- /home/nb/src/aurox/src/cli/dispatch.rs
- /home/nb/src/aurox/src/build/makepkg.rs
- /home/nb/src/aurox/src/pacman/invoke.rs

---

## Status (updated)

### Deviations from the original plan

- **Git backend: `git2` → `gix`.** Measured `git2`/libgit2 reading the AUR pack at ~80 KB/s vs vanilla git at ~10 MB/s on the same connection; `gix` CLI clones in ~8:49 (vs git's 8:12). Switched to pure-Rust `gix 0.83`. No subprocess, no libgit2. Tracked in memory as `project-libgit2-http-slow`.
- **Diagnostic stack: `log` → `tracing`.** `tracing` + `tracing-subscriber` with `EnvFilter`. User-facing colored output (`info`/`step`/`note`/`warn`/`error`/`pkg_list`/`confirm`/bars) is a separate `ui` module — not routed through `tracing`.
- **UI stack:** `indicatif` (bars) + `console` (styling) + `dialoguer` (prompts). `GixProgress` adapter implements `gix::Progress`/`NestedProgress`/`Count`, lazily spawns per-child indicatif bars (created on first `set`/`inc_by`, cleared on `Drop`), detects byte- vs count-units via `display_unit().is_empty()`, switches bar style accordingly. Phase-name → hint mapping ("read pack" → "silent until server finishes packing (~3–5 min server-side, ~2–3 min stream)"), hints rendered in italic gray-244.
- **Sudo: dropped warmup + keepalive.** Replaced with: one upfront batched `pacman -S --asdeps` for repo deps, unprivileged build loop with idempotent rebuild-skip (state.db `last_built_commit_oid == HEAD` AND `.pkg.tar.zst` present → skip makepkg), one final batched `pacman -U` (declinable; re-run replays only the install step). Tracked as `feedback-defer-consolidate-sudo`.
- **Worktrees: `git worktree add` equivalent isn't in gix.** Hand-wrote the on-disk linkage: `<bare>/worktrees/<pkgbase>/{HEAD,commondir,gitdir}` + `<dest>/.git` pointer file. Native `git -C <dest> status` recognizes it; `git -C <bare> worktree list` lists it. Verified by inline integration tests.
- **CLI: hand-rolled passthrough + `clap-derive` hybrid.** Pre-scan argv for an uppercase op letter; pacman-owned ops (`-Q`, `-R`, `-T`, `-D`, `-F`, `-U`) skip clap entirely and forward raw to `pacman` (so clap can't reject unknown short flags like `-Rns`). `-S` family and global flags go through clap → `flags::PacFlags` cluster parser. `--help`/`-Sh` use clap-generated help.
- **`-Syy` added** — pacman-style "force full re-clone of the mirror" (~8–9 min). Useful when the bare clone gets corrupted.

### Done

- Cargo.toml + clippy.toml + lib/bin split.
- `[lints.clippy]` block with `absolute_paths` + pedantic, opt-outs for `module_name_repetitions`/`must_use_candidate`/etc.
- `paths`, `error`, `ui`, `config` (defaults + TOML loader).
- `mirror::clone` (`gix::prepare_clone_bare` + `fetch_only`, custom `GixProgress`).
- `mirror::fetch` (`gix::Remote::prepare_fetch` → `RefUpdate` deltas).
- `mirror::worktree` (hand-written linked-worktree linkage + tree materialization).
- `mirror::sideband` (helper to parse `\r`-separated server progress lines).
- `index::srcinfo` parser (handles split pkgs, arch-suffixed list keys, dedup).
- `index::schema` (rkyv 0.8) + `index::build` (rayon-parallel, structured concurrency: workers borrow `&Path` and reopen `gix::Repository` per closure).
- `index::update::incremental_update` (per-ref upsert/delete; atomic file swap).
- `index::secondary` (by_name + by_provides; parallel regex search).
- `cli` with clap-derive Cli struct, pre-scan, dispatch; pacman passthrough; auto-generated `--help`/`-Sh`.
- `pacman::invoke` (sudo gating heuristic), `pacman::alpm_db` (foreign_pkgs etc.), `pacman::vercmp`.
- `resolver::classify` + `resolver::topo` (Tarjan-style with readable cycle paths) + `resolver::DepGraph::resolve` → `Plan`.
- `build::makepkg` (PKGDEST/SRCDEST/BUILDDIR env), `build::install` (`find_produced`, `extract_pkgname` partition by direct/transitive), `build::review` (line-diff against last-built tree via gix-object), `build::state_db` (sqlite, upsert/prune).
- `build::cmd_install` end-to-end pipeline; `cmd_sysupgrade` (foreign-pkg detect + VCS gate via `--devel`); `cmd_clean` (-Sc / -Scc).
- Tests: 43 inline `#[cfg(test)]` + 3 integration in `tests/fake_mirror.rs`. Shared `aurox::testing::git` helper avoids global gitconfig interference (no `commit.gpgsign` surprises).
- 8 feedback memories captured (style, structured concurrency, sudo deferral, partial-bootstrap recovery, indicatif pitfalls, clap+pacman pattern, shared test helpers, etc.).

### Not yet exercised end-to-end

- A real `aurox -S cower` against the live AUR mirror — clone has been started but never fully completed during a session (~8–9 min on this connection); the post-clone `aurox::index::build::full_build` against 155k branches has not been measured in-tree.
- `pacman::alpm_db` is compile-tested but not run against the real `/var/lib/pacman` (it will be, the moment a `-S` succeeds).
- `aurox -Syu` end-to-end against installed foreign pkgs.
- Split-package install (e.g. `mingw-w64-gcc`).
- `--devel` flow against an actual `-git` pkg.

### Known UX caveats (upstream gix)

- During the server-side packing phase (~3–5 min for the AUR mirror), gix emits no `inc_by` — only the elapsed-time spinner moves. Documented in the leaf-bar hint text.
- After `Resolving` deltas finishes, gix runs `update_refs::update` to write the 155k loose-ref files; that step emits **zero** progress events. The summary briefly shows a "finalizing — writing refs silently (~30 s – 2 min)" hint while bars freeze; this is real disk work, not a hang. A fix would have to be upstream in gitoxide.
