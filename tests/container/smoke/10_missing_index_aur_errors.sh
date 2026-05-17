#!/usr/bin/env bash
# Without the AUR index, an AUR target is unknown → clean error pointing at -Sy.
source /work/tests/container/lib.sh
bootstrap; reset_state

rm -f "$STATE_DIR/index.bin"

gitaur -S --noconfirm test-trivial
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit"; _dump; exit 1; }
assert_stderr_contains "test-trivial"
