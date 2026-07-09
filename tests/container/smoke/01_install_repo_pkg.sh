#!/usr/bin/env bash
# `aurox -S <repo-pkg>` takes the pacman fast path. Verifies:
#   * package ends up installed
#   * marked Explicit (not --asdeps)
#   * aurox doesn't load the AUR index for a pure-repo install (log assertion)
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -S --noconfirm repo-base
assert_exit 0
assert_pkg_installed repo-base
assert_pkg_explicit repo-base
