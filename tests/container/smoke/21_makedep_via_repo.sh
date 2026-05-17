#!/usr/bin/env bash
# Makedep lives in the sync repo, not AUR. It should be pacman-installed
# in the pre-build phase, not via the strata loop.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-make-via-repo
assert_exit 0
assert_pkg_installed test-make-via-repo
assert_pkg_explicit  test-make-via-repo
assert_pkg_installed repo-helper-lib
assert_pkg_asdep     repo-helper-lib
