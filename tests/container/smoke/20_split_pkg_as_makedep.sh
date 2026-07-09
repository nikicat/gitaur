#!/usr/bin/env bash
# Makedep names a split-pkg's pkgname, not its pkgbase. Resolver must map
# pkgname → pkgbase so the strata edge points at the right pkgbase.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-split-as-makedep-client
assert_exit 0
assert_pkg_installed test-split-as-makedep-client
assert_pkg_explicit  test-split-as-makedep-client
assert_pkg_installed test-split-as-makedep-helper-lib
assert_pkg_asdep     test-split-as-makedep-helper-lib
# Sibling pkgname produced by the same pkgbase ends up installed too
# (single pacman -U on the .pkg.tar.zst pair).
assert_pkg_installed test-split-as-makedep-helper-tool
