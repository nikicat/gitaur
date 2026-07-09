#!/usr/bin/env bash
# Regression for the commit-mono-font case. The AUR has two real-world
# pkgbases whose names overlap:
#
#   * pkgbase `commit-mono-font` — pkgnames `otf-commit-mono` + `ttf-commit-mono`
#   * pkgbase `otf-commit-mono`  — pkgname  `otf-commit-mono`
#
# `by_name["otf-commit-mono"]` only stores one entry (HashMap insert-order
# winner). Before the pkgbase-string pinning in `expand_pkgbase_targets`,
# `-S commit-mono-font` rewrote to the pkgname list and the resolver would
# silently classify `otf-commit-mono` into the unrelated pkgbase, building
# both pkgbases and tripping a `pacman -U` file conflict.
#
# Mirrored here with `test-collision-multi` (pkgver 2.0, pkgnames
# test-collision-otf + test-collision-ttf) plus `test-collision-otf` (a
# separate pkgbase at pkgver 1.0). The installed `Version` field
# distinguishes which pkgbase actually produced the file pacman saw — only
# the multi pkgbase's 2.0 must reach the install transaction.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-collision-multi
assert_exit 0

# Both split pkgnames from the multi pkgbase must be installed.
assert_pkg_installed test-collision-otf
assert_pkg_installed test-collision-ttf

# Crucial: the installed `test-collision-otf` must be the 2.0 build from
# `test-collision-multi`, not the standalone 1.0 build. If the bug
# regressed, pacman would have ended up with the 1.0 file (or hit a file
# conflict between the two — also a failure).
ver="$(pacman -Qi test-collision-otf 2>/dev/null | awk -F': +' '/^Version/{print $2}')"
[[ "$ver" == 2.0-1 ]] || {
    echo "expected test-collision-otf 2.0-1, got '$ver' — the standalone test-collision-otf pkgbase leaked into the build plan" >&2
    pacman -Qi test-collision-otf >&2 || true
    exit 1
}

# Build artifacts: only `test-collision-multi` should have a worktree.
# `test-collision-otf` (the standalone pkgbase) must NOT be touched — its
# absence proves the resolver's plan stayed scoped to one pkgbase.
[[ -d ~/.local/state/aurox/pkgs/test-collision-multi ]] || {
    echo "expected test-collision-multi worktree under ~/.local/state/aurox/pkgs/" >&2
    exit 1
}
[[ ! -d ~/.local/state/aurox/pkgs/test-collision-otf ]] || {
    echo "test-collision-otf pkgbase was built — collision regressed" >&2
    ls -la ~/.local/state/aurox/pkgs/ >&2
    exit 1
}
