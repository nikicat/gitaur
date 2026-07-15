#!/usr/bin/env bash
# -Syy proposes a from-scratch re-clone; declining must leave the existing
# mirror + index untouched (regression: the wipe once ran before consent).
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0
[[ -d "$STATE_DIR/aur" ]] || { echo "bootstrap expected" >&2; _dump >&2; exit 1; }

aurox_input "n" -Syy
assert_exit 0
assert_stderr_contains "re-clones the AUR mirror from scratch"
[[ -d "$STATE_DIR/aur" ]] || { echo "declined -Syy must keep the mirror" >&2; _dump >&2; exit 1; }

# The index is still servable — search works without another fetch.
aurox -Ss "^test-trivial$"
assert_exit 0
assert_stdout_contains "aur/test-trivial"
