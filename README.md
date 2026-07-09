# aurox

[![CI](https://github.com/nikicat/aurox/actions/workflows/ci.yml/badge.svg)](https://github.com/nikicat/aurox/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![AUR version](https://img.shields.io/aur/version/aurox)](https://aur.archlinux.org/packages/aurox)

A yay-like AUR helper backed by the [`github.com/archlinux/aur`](https://github.com/archlinux/aur) mirror — no dependency on the `aurweb` RPC.

`aur.archlinux.org` has spotty uptime; the GitHub mirror is a single ~2 GiB monorepo with one branch per package. `aurox` clones it once, builds a zero-copy index from every `.SRCINFO`, and uses that for search, info, and dependency resolution. Refreshes are incremental fetches.

## Status

Early — works end-to-end: AUR search/info/install (`-S`), recursive dep resolution, PKGBUILD review, and an interactive shell (`aurox` with no args) for staging + upgrading + applying. Repo/system upgrades go through `pacman` (`-Syu` is a passthrough). **Not** packaged for the AUR yet; expect rough edges.

## Install

From source (requires `pacman`, `git`, `base-devel`, `pkgconf`, `rustup`):

```sh
git clone https://github.com/nikicat/aurox
cd aurox
cargo install --path . --locked
```

## Usage

`aurox` accepts pacman's flag syntax. Operations it doesn't own (`-Q`, `-R`, `-T`, `-D`, `-F`, `-U`, and `-Su` system upgrades) are forwarded to `pacman` unchanged, so you can use it as a drop-in replacement.

| Command               | What it does                                                  |
| --------------------- | ------------------------------------------------------------- |
| `aurox`                | Open the interactive shell (search · stage · `upgrade` · `apply`) |
| `aurox -S <pkg>...`    | Install AUR packages (recursive deps, batched sudo)           |
| `aurox -Sy`            | Incremental fetch of the AUR mirror                           |
| `aurox -Syy`           | Force a full re-clone (~8–9 min)                              |
| `aurox -Syu`           | Forwarded to `pacman -Syu` — AUR upgrades live in the shell's `upgrade` |
| `aurox -Ss <regex>`    | Search the AUR by name / desc / provides                      |
| `aurox -Si <pkg>`      | Show package info                                             |
| `aurox -Sc` / `-Scc`   | Remove built worktrees + pass `-Sc`/`-Scc` through to `pacman` |
| `aurox -Rns <pkg>`     | Forwarded to `pacman` unchanged                               |

AUR upgrades are an interactive flow now: run `aurox` (no args) to open the shell, then `upgrade` to stage the available AUR + repo upgrades, `review`/`approve` the AUR ones, and `apply`. The explicit `-Syu` flag is a plain `pacman -Syu` passthrough.

Global flags: `--devel` (include `-git`/`-svn`/`-hg`/`-bzr` when the shell's `upgrade` computes candidates), `--noconfirm`, `--asdeps`, `--color {auto,always,never}`.

### Examples

```sh
aurox -S yay-bin
aurox --devel              # open the shell; `upgrade` then includes -git/-svn pkgs
aurox -Ss '^claude-'
aurox -Rns aurox          # forwarded to pacman
RUST_LOG=aurox=debug aurox -Sy
```

## Configuration

Optional `~/.config/aurox/config.toml`. All fields default to sensible values:

```toml
mirror_url           = "https://github.com/archlinux/aur.git"
build_dir            = "~/.local/state/aurox/pkgs"
index_threads        = 4
refresh_max_age_secs = 3600
color                = "auto"
makepkg_path         = "makepkg"
makepkg_args         = ["-s", "--noconfirm", "--needed"]
privilege_escalator  = "sudo"      # or "doas" / "run0"
devel                = false
review_default       = "prompt"    # or "skip" / "always-show"
aur_approval         = "review"    # or "auto" — auto stages AUR pkgs pre-approved
```

The shell's approval gate: `review` (default) makes every staged AUR package
need `review`/`approve` before `apply` runs it; `auto` stages them pre-approved.
If unset, `review_default = "skip"` still auto-approves (legacy behavior).

## Layout on disk

```
~/.local/state/aurox/
├── aur/              bare clone of the AUR mirror (~2 GiB)
├── pkgs/<pkgbase>/   per-pkgbase git worktree + cached .pkg.tar.zst, kept until -Sc
├── index.bin         rkyv-archived index, mmap'd at load (~60–80 MB)
├── logs/             per-run debug logs (aurox-*.log), last 10 runs kept
└── traces/           per-run Chrome/Perfetto span traces (aurox-*.json)

~/.config/aurox/config.toml  optional
```

The `logs/` files are always written at `debug` level regardless of console
verbosity. `RUST_LOG` (e.g. `RUST_LOG=aurox=debug`) raises only the *console*
tracing level — it does not change what lands in `logs/`.

The `traces/` files drop straight into [ui.perfetto.dev](https://ui.perfetto.dev),
but the `aurox-trace` helper answers "where did the time in span X go?" from the
terminal without opening a browser. With no argument it reads the newest trace:

```
aurox-trace                      # spans aggregated by total self time
aurox-trace tree                 # full per-thread containment tree
aurox-trace tree --span receive  # just the subtree(s) under `receive`
aurox-trace --min-ms 50 tree     # hide spans shorter than 50 ms
```

`self` time is each span's wall time minus its children — the un-instrumented
cost that lives directly in that span. Pass `--file <path>` for a specific trace.

## How it differs from yay / paru

- **No `aurweb` RPC.** All metadata comes from the GitHub mirror clone.
- **Incremental refresh.** `git fetch` reports changed refs; only those are re-indexed.
- **Zero-copy index.** `index.bin` is a `rkyv` archive, mmap'd directly — no parse step on load.
- **One sudo prompt per install.** Repo deps go in via a single batched `pacman -S`; built `.pkg.tar.zst`s go in via a single batched `pacman -U` at the very end. No keepalive loop.
- **Idempotent builds.** A pkgbase whose worktree already holds a `.pkg.tar.zst` at the AUR index's exact `[epoch:]pkgver-pkgrel` for every required pkgname is skipped, so re-running after declining the install just replays the install step. No sidecar DB — the artifact filename is the cache key.

## Development

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs all of the above on an `archlinux:latest` container.

In addition to the in-process Rust tests there is a black-box container
suite under `tests/container/` that runs every aurox command against
real `pacman` + `makepkg` inside an ephemeral Arch userspace (podman
default, docker via `CONTAINER=docker`). It is the only place where the
multi-process build pipeline — sudo gating, asdeps flips, build
failure isolation, makepkg log capture — is exercised end-to-end. CI runs
it (both the `smoke` and `extended` tiers) as its own gating job alongside
the Rust tests.

```sh
bash tests/container/run.sh                 # smoke tier (~30 s on 8 cores)
bash tests/container/run.sh --rebuild smoke # bust image cache after fixture changes
```

Full details: [`docs/TESTING.md`](docs/TESTING.md).

A `.pre-commit-config.yaml` is checked in to catch the cheap failures
(`cargo fmt --check`, `taplo fmt --check`, `taplo lint`) before they round-trip
through CI. One-time setup per clone:

```sh
cargo install prek    # or: pacman -S prek (once it lands in extra)
prek install
```

The hook stays sub-second; `cargo clippy` and `cargo test` are deliberately
left out — run them yourself before pushing.

## License

[MIT](LICENSE).
