#!/usr/bin/env bash
# checkdepends count as build-time and drive strata.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-checkdep
assert_exit 0
assert_pkg_installed test-checkdep
assert_pkg_explicit  test-checkdep
assert_pkg_installed test-make-chain-c
assert_pkg_asdep     test-make-chain-c
