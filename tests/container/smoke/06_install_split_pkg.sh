#!/usr/bin/env bash
# Split PKGBUILD produces two .pkg.tar.zst's; both should end up installed
# when one of the split pkgnames is the direct target.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-split-extras
assert_exit 0
assert_pkg_installed test-split-extras
assert_pkg_explicit test-split-extras
# test-split-core is an intra-split dependency: produced by the same pkgbase.
assert_pkg_installed test-split-core
