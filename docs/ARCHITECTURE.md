# gitaur architecture

This is a maintainer's map of how `gitaur` is wired. For user-facing flags
see `README.md`; for the test suite see `docs/TESTING.md`; for profiling
see `docs/PROFILING.md`.

## The 30-second tour

`gitaur` is a pacman-compatible CLI that resolves and builds AUR packages
against a local clone of [`github.com/archlinux/aur`](https://github.com/archlinux/aur)
— a single bare repo where every package is its own `refs/heads/<pkgbase>`
branch (~154 k of them, ~2 GiB pack). Three big moving parts:

1. **The mirror** (`src/mirror/`) — bare clone on disk, refreshed via
   incremental gix fetches. `~/.local/state/gitaur/aur`.
2. **The index** (`src/index/`) — rkyv-archived blob mapping pkgname →
   pkgbase + deps + provides + version. One file, mmapped at load.
   `~/.local/state/gitaur/index.bin`.
3. **The build pipeline** (`src/build/`) — resolves a `-S` target list
   into a `Plan`, drives `makepkg` per pkgbase in stratified order, then
   `pacman -U`'s the results.

Anything pacman owns (`-Q`, `-R`, `-T`, `-D`, `-F`, `-U`, and the
`pacman.conf` it reads) is forwarded verbatim. `gitaur` only owns `-S`
family operations and the AUR-related half of `-Syu`.

## Module map

```
src/
├── cli/             argv pre-scan + clap + dispatch to handlers
│   ├── mod.rs       entry point; pre-scan routes pacman-owned ops
│   ├── flags.rs     pacman-style clustered flag parser (-Syyu → S,y,y,u)
│   └── dispatch.rs  routes to mirror / index / build subcommands
│
├── mirror/          AUR mirror lifecycle
│   ├── mod.rs       cmd_refresh: clone-or-fetch + index update
│   ├── clone.rs     gix bare clone (custom refspec — see TESTING.md)
│   ├── fetch.rs     incremental gix fetch; emits RefUpdate deltas
│   ├── worktree.rs  per-pkgbase build worktrees via git linked worktrees
│   └── sideband.rs  parse libgit2 sideband for nicer progress UI
│
├── index/           rkyv-archived AUR catalog
│   ├── mod.rs       load/save/search/info
│   ├── schema.rs    IndexFile + IndexEntry definitions
│   ├── build.rs     full_build: parallel parse of every .SRCINFO blob
│   ├── update.rs    incremental_update: applies RefUpdate deltas
│   ├── secondary.rs by_name / by_provides hash tables (built post-load)
│   └── srcinfo.rs   tiny .SRCINFO parser
│
├── resolver/        plan a `-S` invocation
│   ├── mod.rs       resolve: BFS into Plan + Kahn strata
│   ├── classify.rs  installed / repo / aur(idx) / missing
│   └── topo.rs      sort (flat) + strata (Kahn layered)
│
├── build/           the install pipeline
│   ├── mod.rs       cmd_install + cmd_sysupgrade + cmd_clean
│   ├── makepkg.rs   spawn makepkg with PKGDEST/SRCDEST/BUILDDIR pinned
│   ├── install.rs   .pkg.tar.zst discovery + pkgname extraction
│   ├── review.rs    PKGBUILD diff review prompt
│   └── state_db.rs  SQLite: last-built commit_oid per pkgbase
│
├── pacman/          everything that wraps pacman / libalpm
│   ├── alpm_db.rs   open Alpm + PacmanIndex snapshot (sync DBs)
│   ├── invoke.rs    spawn `pacman` (with sudo escalation)
│   └── vercmp.rs    pacman version comparison
│
├── config/          ~/.config/gitaur/config.toml + defaults
├── error.rs         single Error enum (anyhow-free; we own the variants)
├── logging.rs       per-run rotating debug log under state_dir/logs
├── paths.rs         XDG-aware state/config path helpers
├── ui.rs            pacman/yay-style banners, prompts, progress bars
└── testing.rs       #[doc(hidden)] shared test helpers (git CLI runner)
```

## Data flow: `gitaur -S <pkg>` end-to-end

```
argv ──► cli::pre-scan ──► clap ──► dispatch::handle_s
                                                │
                                                ▼
                       ┌──── rayon::join ──────────────────┐
                       │                                   │
                       ▼                                   ▼
           PacmanIndex::build (alpm)        IndexFile::load + Secondary
                       │                                   │
                       └──────► resolver::resolve ◄────────┘
                                       │
                                       ▼
                       ┌──── classify per target ────┐
                       ▼              ▼              ▼
                   Installed       Repo          Aur(idx)    Missing → error
                   (skip)       direct/        BFS deps,
                                transitive_repo build edges
                                       │
                                       ▼
                            topo::sort   (cycle check, full graph)
                            topo::strata (Kahn over makedepends+checkdepends)
                                       │
                                       ▼
                                     Plan
                  { direct_repo, transitive_repo, aur_strata, direct_targets }
                                       │
                                       ▼
                   ┌── plan.aur_strata.is_empty()? ──┐
                  yes                                no
                   │                                  │
                   ▼                                  ▼
           pacman -S (one call)                 install_repo_phase
           — pacman's UI verbatim                 (pacman -S direct, -S --asdeps transitive)
                                                       │
                                                       ▼
                                                run_aur_pipeline
                                                  for each stratum:
                                                    build_one(makepkg)
                                                    install_stratum(pacman -U)
                                                  finally:
                                                    pacman -D --asdeps <marks>
```

## Key design choices

### Why two phases of dep resolution (cycle check + strata)?

`topo::sort` runs over the **full** dep graph (depends + makedepends +
checkdepends) purely to reject cycles — even a cycle through plain runtime
`depends` is unbuildable. `topo::strata` then runs over **makedepends +
checkdepends only**: those are the build-time constraints that decide
when a pkg's makepkg is allowed to run. Runtime `depends` get resolved
together at the eventual `pacman -U`, which is allowed to satisfy intra-
batch deps. The split is what lets siblings in the same stratum build
without one needing the other installed first.

### Why a precomputed `PacmanIndex`?

`alpm::Alpm` is `Send` but not `Sync`. It wraps a C handle that isn't
thread-safe. Anything that wants to classify deps in parallel — and we
do via rayon — can't share `&Alpm`. So `PacmanIndex::build(&Alpm)`
snapshots the local + sync DBs into owned `HashMap`/`HashSet` once;
classification then becomes pure data, parallelisable, and cheap.

### Why per-worker `gix::Repository` clones in `full_build`?

`gix::Repository` is `Send` but **not** `Sync` — it carries interior
`RefCell`s for object / pack / zlib caches. So the rayon workers in
`index::build::full_build` can't share a single `&mirror.repo`. The
pattern is `par_iter().map_init(|| repo.clone(), op)`: each worker
thread takes one cheap structural clone (shares the underlying `Arc`'d
object DB + refs; only the per-thread caches are fresh) and reuses it
across every branch it pulls. A `Mutex` wraps the seed clone so the
`map_init` init closure (which must be `Sync`) can pull a fresh handle
without capturing `&Repository`. Lock contention is bounded by
`cfg.index_threads` because init runs once per worker thread, not per
branch.

What you must **not** do: `gix::open(&path)` inside the worker closure.
Reopening reparses config + rescans refs + rediscovers alternates per
branch and dominates wall time (observed: ~2.2 ms/branch ⇒ 5+ minutes
on the 150 k-branch AUR mirror). The regression test in
`tests/build_worker_shares_repo.rs` asserts the
`WORKER_REPO_OPENS` counter in `index::build` stays at zero; bump it
from any future worker-side `gix::open` so the test catches the regression.

### Why `makepkg -d` (skip dep checks) instead of `-s`?

`makepkg -s` tries to install missing deps via `pacman -S`, which can
only fetch from sync repos. For AUR-only deps the fetch fails — `pacman`
doesn't know about them. So gitaur:

1. Pre-installs all **repo** deps (direct + transitive) via `pacman -S`.
2. Pre-installs all **AUR makedeps + checkdeps** stratum-by-stratum via
   `pacman -U` after each stratum's builds.
3. Tells `makepkg` to skip its own checks (`-d`).

Runtime `depends` are satisfied later by the same stratum's `pacman -U`
resolving intra-batch.

### Why one big `IndexFile` blob instead of a SQLite catalog?

Search-and-info workloads are 100 % scan-the-whole-thing. `rkyv` lets us
mmap the on-disk blob and dereference fields with zero copies; `rayon`
parallelises the regex scan across ~154 k entries trivially. A SQLite
catalog would force per-row deserialization and per-query index lookups
that don't help when most queries are regex over `pkgname` + `pkgdesc`.

The catalog is rebuilt incrementally — `index::update::incremental_update`
applies the `RefUpdate` deltas produced by `mirror::fetch::incremental_fetch`,
so a `gitaur -Sy` doesn't re-parse the 99 % of pkgbases that didn't move.

### Why a state DB (SQLite) for builds?

`build/state_db.rs` records `last_built_commit_oid` per pkgbase. Lets us
skip `makepkg` when the worktree is already at that commit AND the
`.pkg.tar.zst` is still on disk — idempotent re-runs after a declined
`pacman -U` or interrupted install.

### Why gix instead of libgit2 / shelling out to `git`?

- libgit2 HTTP is ~100× slower than the git CLI on the AUR mirror's pack
  (see `memory/project_libgit2_http_slow.md` style of finding).
- Subprocess `git` is fine for clone/fetch but doesn't let us hook
  progress / per-ref deltas the way we want for the UI.

So gix for fetch + index walks (pure Rust, no subprocess), with two
specific quirks worth knowing:

1. `gix::prepare_clone_bare` defaults to a non-bare refspec
   (`+refs/heads/*:refs/remotes/origin/*`). We override via
   `replace_refspecs` so refs land under `refs/heads/*` — see
   `tests/clone_refs_layout.rs` for the regression test.
2. Bootstrap clone over HTTPS to `github.com` is slow at the negotiation
   stage; relies on PRs #2604/#2605 against gitoxide.

### Argv parsing — why both clap AND PacFlags?

Pacman accepts flags freely on either side of the operation
(`pacman --noconfirm -S foo` and `pacman -S --noconfirm foo` both work).
clap with `trailing_var_arg + allow_hyphen_values` is needed so flags
unknown to gitaur (e.g. pacman's `-Rns`) don't trip clap. The cost: any
flag after `-S` lands in the trailing var arg and never reaches
`cli.noconfirm`. `cli/flags.rs` re-parses argv into `PacFlags`; `dispatch`
ORs the two sources together. If you add a new global flag, you'll need
to plumb it through both.

## Where state lives

| Path                                          | Owner            | Contents                              |
| --------------------------------------------- | ---------------- | ------------------------------------- |
| `~/.local/state/gitaur/aur/`                  | gix bare clone   | AUR mirror, branches under `refs/heads/<pkgbase>` |
| `~/.local/state/gitaur/index.bin`             | `index::save`    | rkyv-archived `IndexFile`             |
| `~/.local/state/gitaur/pkgs/<pkgbase>/`       | linked worktrees | per-pkgbase build dir                 |
| `~/.local/state/gitaur/state.db`              | rusqlite         | last-built commit per pkgbase         |
| `~/.local/state/gitaur/logs/`                 | logging          | last 10 invocation logs               |
| `~/.config/gitaur/config.toml`                | user             | overrides for `config::defaults`      |

## Common gotchas for new maintainers

- **`alpm` mutability**: do NOT hold `&Alpm` across rayon workers; build
  a `PacmanIndex` first.
- **`gix::Repository` is `Send` but not `Sync`**: parallel workers must
  hold their own clone (see `full_build` and its `WORKER_REPO_OPENS`
  regression seam). Never `gix::open` inside a per-branch worker closure.
- **`gix` refs under `refs/remotes/origin/*`**: only the bootstrap clone
  is affected (see custom refspec in `clone.rs`). Subsequent fetches
  write to `refs/heads/*` because that's what the bare config records.
- **makepkg refuses to run as root**: the build worktree must be owned
  by a non-root user. In CI / containers this means an unprivileged
  `builder` user with passwordless sudo for the pacman calls.
- **Sudo is consolidated, not cached by gitaur**: we don't run
  `sudo -v` keepalives. We assume the OS sudo cache (5-15 min) bridges
  the per-stratum prompts.
- **Don't add `aur_order: Vec<String>`**: it was replaced by
  `aur_strata: Vec<Vec<String>>`. Use `plan.aur_order()` for a flat
  view; the strata structure is load-bearing for the build pipeline.
