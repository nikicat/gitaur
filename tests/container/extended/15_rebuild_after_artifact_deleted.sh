#!/usr/bin/env bash
# The complement of smoke/43's cached-skip: "already built" is derived
# purely from the on-disk `.pkg.tar.zst` (no sidecar DB), so deleting the
# artifact must flip the same check the other way — the next `-S` reruns
# makepkg instead of trusting stale state.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-trivial
assert_exit 0
assert_pkg_installed test-trivial

pkg_file=$(ls "$STATE_DIR"/pkgs/test-trivial/*.pkg.tar.zst 2>/dev/null | head -1)
[[ -n "$pkg_file" ]] || { echo "no built artifact found" >&2; exit 1; }

# Wipe the artifact and the installed pkg; only the worktree remains.
rm -f "$STATE_DIR"/pkgs/test-trivial/*.pkg.tar.zst
sudo pacman -R --noconfirm test-trivial >/dev/null

aurox -S --noconfirm test-trivial
assert_exit 0
assert_stderr_not_contains "already built"
assert_pkg_installed test-trivial
new_file=$(ls "$STATE_DIR"/pkgs/test-trivial/*.pkg.tar.zst 2>/dev/null | head -1)
[[ -n "$new_file" ]] || { echo "rebuild produced no artifact" >&2; _dump >&2; exit 1; }
