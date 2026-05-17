#!/usr/bin/env bash
# A ↔ B cycle (regular depends) — resolver's full-dep cycle check rejects.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-cycle-a
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit"; _dump; exit 1; }
assert_stderr_contains "cycle"
