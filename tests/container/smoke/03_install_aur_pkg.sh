#!/usr/bin/env bash
# `gitaur -S <aur-pkg>` runs full build pipeline against the mock mirror.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
assert_exit 0

gitaur -S --noconfirm test-trivial
assert_exit 0
assert_pkg_installed test-trivial
assert_pkg_explicit test-trivial
