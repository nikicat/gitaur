#!/usr/bin/env bash
# Declining the sudo elevation gate *after* the AUR build must keep the
# built `.pkg.tar.zst` — smoke/43's idempotency comment promises declined
# installs are retried cheaply, and this is the test of that promise: the
# retry hits the "already built" path instead of rerunning makepkg.
#
# Interactive prompt order for `-S test-trivial` (explicit-only plan, so no
# plan-level "Proceed?" — smoke/51): the PKGBUILD review reads one line
# (empty = default = approve), makepkg builds, then the elevation gate's
# "Continue?" reads the "n".
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox_input $'\nn' -S test-trivial
assert_exit 1
assert_stderr_contains "about to elevate"
assert_pkg_not_installed test-trivial

pkg_file=$(ls "$STATE_DIR"/pkgs/test-trivial/*.pkg.tar.zst 2>/dev/null | head -1)
[[ -n "$pkg_file" ]] || {
    echo "declined install must keep the built artifact" >&2
    ls -la "$STATE_DIR/pkgs/test-trivial/" >&2 || true
    _dump >&2
    exit 1
}

# The retry reuses the artifact — no second makepkg run.
aurox -S --noconfirm test-trivial
assert_exit 0
assert_stderr_contains "test-trivial: already built"
assert_pkg_installed test-trivial
