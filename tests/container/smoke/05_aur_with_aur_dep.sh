#!/usr/bin/env bash
# Transitive AUR resolution + topo build order + --asdeps for the dependency.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-with-aur-dep
assert_exit 0
assert_pkg_installed test-with-aur-dep
assert_pkg_explicit test-with-aur-dep
assert_pkg_installed test-trivial
assert_pkg_asdep test-trivial
