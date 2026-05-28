#!/usr/bin/env bash
# Without a -Sy ever run, repo targets must still install — the AUR index
# is optional and pacman-resolvable names take the fast path.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Ensure no AUR index on disk.
rm -f "$STATE_DIR/index.bin"

gaur -S --noconfirm repo-base
assert_exit 0
assert_pkg_installed repo-base
