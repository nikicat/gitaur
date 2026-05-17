#!/usr/bin/env bash
# Diamond makedep graph: d ← {b,c} ← a. b and c can be built in parallel
# within one stratum and must both be installed before d's makepkg.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-make-diamond-d
assert_exit 0
assert_pkg_installed test-make-diamond-a
assert_pkg_installed test-make-diamond-b
assert_pkg_installed test-make-diamond-c
assert_pkg_installed test-make-diamond-d
assert_pkg_explicit  test-make-diamond-d
assert_pkg_asdep     test-make-diamond-a
assert_pkg_asdep     test-make-diamond-b
assert_pkg_asdep     test-make-diamond-c
