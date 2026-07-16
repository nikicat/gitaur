#!/usr/bin/env bash
# `-Rns` is pacman's job: aurox forwards the exact cluster (elevated) and
# pacman removes the target plus its now-unneeded deps. Seed: repo-with-dep
# pulls repo-helper-lib as a dependency; removing the former with `-Rns`
# must take the orphaned dep with it.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -S --noconfirm repo-with-dep
assert_exit 0
assert_pkg_installed repo-with-dep
assert_pkg_installed repo-helper-lib
assert_pkg_asdep repo-helper-lib

aurox -Rns --noconfirm repo-with-dep
assert_exit 0
assert_stderr_contains "sudo pacman -Rns"
assert_pkg_not_installed repo-with-dep
assert_pkg_not_installed repo-helper-lib
