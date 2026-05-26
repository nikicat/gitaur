# gitaur

[![CI](https://github.com/nikicat/gitaur/actions/workflows/ci.yml/badge.svg)](https://github.com/nikicat/gitaur/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![AUR version](https://img.shields.io/aur/version/gitaur)](https://aur.archlinux.org/packages/gitaur)

A yay-like AUR helper backed by the [`github.com/archlinux/aur`](https://github.com/archlinux/aur) mirror — no dependency on the `aurweb` RPC.

`aur.archlinux.org` has spotty uptime; the GitHub mirror is a single ~2 GiB monorepo with one branch per package. `gitaur` clones it once, builds a zero-copy index from every `.SRCINFO`, and uses that for search, info, and dependency resolution. Refreshes are incremental fetches.

## Status

Early — works end-to-end (search, info, install, `-Syu`, dep resolution, PKGBUILD review), but is **not** packaged for the AUR yet. Expect rough edges.

## Install

From source (requires `pacman`, `git`, `base-devel`, `pkgconf`, `rustup`):

```sh
git clone https://github.com/nikicat/gitaur
cd gitaur
cargo install --path . --locked
```

## Usage

`gitaur` accepts pacman's flag syntax. Operations it doesn't own (`-Q`, `-R`, `-T`, `-D`, `-F`, `-U`) are forwarded to `pacman` unchanged, so you can use it as a drop-in replacement.

| Command                 | What it does                                                  |
| ----------------------- | ------------------------------------------------------------- |
| `gitaur`                | Refresh the AUR mirror + index (same as `-Sy`)                |
| `gitaur -S <pkg>...`    | Install AUR packages (recursive deps, batched sudo)           |
| `gitaur -Sy`            | Incremental fetch of the AUR mirror                           |
| `gitaur -Syy`           | Force a full re-clone (~8–9 min)                              |
| `gitaur -Syu`           | `pacman -Syu`, then AUR upgrades                              |
| `gitaur -Ss <regex>`    | Search the AUR by name / desc / provides                      |
| `gitaur -Si <pkg>`      | Show package info                                             |
| `gitaur -Sc` / `-Scc`   | Remove built worktrees + pass `-Sc`/`-Scc` through to `pacman` |
| `gitaur -Rns <pkg>`     | Forwarded to `pacman` unchanged                               |

Global flags: `--devel` (include `-git`/`-svn`/`-hg`/`-bzr` in `-Syu`), `--noconfirm`, `--asdeps`, `--color {auto,always,never}`.

### Examples

```sh
gitaur -S yay-bin
gitaur -Syu --devel
gitaur -Ss '^claude-'
gitaur -Rns gitaur          # forwarded to pacman
RUST_LOG=gitaur=debug gitaur -Sy
```

## Configuration

Optional `~/.config/gitaur/config.toml`. All fields default to sensible values:

```toml
mirror_url           = "https://github.com/archlinux/aur.git"
build_dir            = "~/.local/state/gitaur/pkgs"
index_threads        = 4
refresh_max_age_secs = 3600
color                = "auto"
makepkg_path         = "makepkg"
makepkg_args         = ["-s", "--noconfirm", "--needed"]
privilege_escalator  = "sudo"      # or "doas" / "run0"
devel                = false
review_default       = "prompt"    # or "skip" / "always-show"
```

## Layout on disk

```
~/.local/state/gitaur/
├── aur/              bare clone of the AUR mirror (~2 GiB)
├── pkgs/<pkgbase>/   per-pkgbase git worktree + cached .pkg.tar.zst, kept until -Sc
├── index.bin         rkyv-archived index, mmap'd at load (~60–80 MB)
├── logs/             per-run debug logs (gitaur-*.log), last 10 runs kept
└── traces/           per-run Chrome/Perfetto span traces (gitaur-*.json)

~/.config/gitaur/config.toml  optional
```

The `logs/` files are always written at `debug` level regardless of console
verbosity. `RUST_LOG` (e.g. `RUST_LOG=gitaur=debug`) raises only the *console*
tracing level — it does not change what lands in `logs/`.

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
suite under `tests/container/` that runs every gitaur command against
real `pacman` + `makepkg` inside an ephemeral Arch userspace (podman
default, docker via `CONTAINER=docker`). It is the only place where the
multi-process build pipeline — sudo gating, asdeps flips, build
failure isolation, makepkg log capture — is exercised end-to-end.

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
