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
| `gitaur -Sc` / `-Scc`   | Clean built worktrees; `-Scc` also drops the build state DB   |
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
├── pkgs/<pkgbase>/   per-pkgbase git worktree, kept until -Sc
├── index.bin         rkyv-archived index, mmap'd at load (~60–80 MB)
└── state.db          SQLite: last-built commit OID per pkgbase

~/.config/gitaur/config.toml  optional
```

## How it differs from yay / paru

- **No `aurweb` RPC.** All metadata comes from the GitHub mirror clone.
- **Incremental refresh.** `git fetch` reports changed refs; only those are re-indexed.
- **Zero-copy index.** `index.bin` is a `rkyv` archive, mmap'd directly — no parse step on load.
- **One sudo prompt per install.** Repo deps go in via a single batched `pacman -S`; built `.pkg.tar.zst`s go in via a single batched `pacman -U` at the very end. No keepalive loop.
- **Idempotent builds.** A pkgbase whose state DB OID matches the branch tip and whose `.pkg.tar.zst` is still on disk is skipped, so re-running after declining the install just replays the install step.

## Development

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs all of the above on an `archlinux:latest` container.

## License

[MIT](LICENSE).
