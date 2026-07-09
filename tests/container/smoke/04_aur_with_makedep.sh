#!/usr/bin/env bash
# AUR pkg with a sync-repo makedep: aurox must install the makedep via
# pacman as --asdeps before driving makepkg, and the makedep stays as-dep
# after the AUR pkg is installed.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-with-makedep
assert_exit 0
assert_pkg_installed test-with-makedep
assert_pkg_explicit test-with-makedep
assert_pkg_installed repo-helper-lib
assert_pkg_asdep repo-helper-lib
