#!/usr/bin/env bash
# The accepted branch of `-Syy`: consenting to the "re-clones the AUR
# mirror from scratch" question must actually wipe the bare mirror — a
# marker file planted inside it disappears (smoke/64 pins the declined
# branch, where the same marker would survive untouched) — and rebuild
# the index from the fresh clone, leaving search fully servable.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0
[[ -d "$STATE_DIR/aur" ]] || { echo "bootstrap expected" >&2; _dump >&2; exit 1; }
marker="$STATE_DIR/aur/reclone-canary"
touch "$marker"

aurox_input "y" -Syy
assert_exit 0
assert_stderr_contains "re-clones the AUR mirror from scratch"
assert_stderr_contains "building index"

[[ ! -e "$marker" ]] || {
    echo "accepted -Syy must wipe + re-clone the mirror (canary survived)" >&2
    _dump >&2
    exit 1
}

aurox -Ss '^test-trivial$'
assert_exit 0
assert_stdout_contains "aur/test-trivial"
