#!/usr/bin/env bash
# `aurox -S <aur-pkg>` runs full build pipeline against the mock mirror.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

aurox -S --noconfirm test-trivial
assert_exit 0
assert_pkg_installed test-trivial
assert_pkg_explicit test-trivial

# Once installed, `-Si` picks the on-disk size up from the localdb (an AUR
# pkgbase has no syncdb to quote one from beforehand).
aurox -Si test-trivial
assert_exit 0
assert_stdout_contains "Installed Size  : "
