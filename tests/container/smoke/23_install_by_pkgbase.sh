#!/usr/bin/env bash
# `-S <pkgbase>` (yay-style) when no pkgname equals the pkgbase. test-split's
# pkgbase is `test-split` but its pkgnames are `test-split-core` /
# `test-split-extras`; the by_pkgbase fallback in Secondary lets the resolver
# find the entry. Both split pkgs end up installed because building the
# pkgbase produces both.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-split
assert_exit 0
assert_pkg_installed test-split-core
assert_pkg_installed test-split-extras

# -Si by pkgbase should also resolve via the same fallback.
aurox -Si test-split
assert_exit 0
assert_stdout_contains "Name            : test-split"
