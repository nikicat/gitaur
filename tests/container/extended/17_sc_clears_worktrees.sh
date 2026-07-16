#!/usr/bin/env bash
# `-Sc` forwards to `pacman -Sc` (elevated) and then clears aurox's own
# per-pkgbase build state (`$STATE_DIR/pkgs`): worktrees, artifacts, logs.
# The installed package must survive — clean is about caches, not installs
# — and a later `-S` must be able to rebuild from a freshly-created
# worktree (no stale worktree metadata left behind in the mirror).
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-trivial
assert_exit 0
[[ -d "$STATE_DIR/pkgs/test-trivial" ]] || { echo "expected a build dir" >&2; exit 1; }

aurox -Sc --noconfirm
assert_exit 0
assert_stderr_contains "sudo pacman -Sc"
assert_stderr_contains "removing per-pkgbase worktrees"

[[ -d "$STATE_DIR/pkgs" ]] || { echo "pkgs root should be recreated" >&2; _dump >&2; exit 1; }
leftover=$(find "$STATE_DIR/pkgs" -mindepth 1 | head -1)
[[ -z "$leftover" ]] || { echo "pkgs root not empty after -Sc: $leftover" >&2; _dump >&2; exit 1; }
assert_pkg_installed test-trivial

# Post-clean rebuild works: a fresh worktree is checked out cleanly.
aurox -S --noconfirm test-epoch
assert_exit 0
assert_pkg_installed test-epoch
