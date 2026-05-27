#!/usr/bin/env bash
# After a refresh, gitaur writes a commit-graph for the local AUR mirror clone
# so the *next* fetch's negotiation reads commit times from an mmap'd file
# instead of inflating every commit from the pack. This exercises that end to
# end: the graph must be written, must verify clean, and a second refresh must
# still succeed with the graph present (it gets read, not corrupted).
source /work/tests/container/lib.sh
bootstrap; reset_state

AUR_DIR="$STATE_DIR/aur"
INFO_DIR="$AUR_DIR/objects/info"

# First refresh bootstraps the bare clone + index, then seeds the commit-graph.
gitaur -Sy
assert_exit 0

# A commit-graph landed — either a single `commit-graph` file or a `--split`
# chain under `commit-graphs/`.
if [[ ! -f "$INFO_DIR/commit-graph" && ! -d "$INFO_DIR/commit-graphs" ]]; then
    echo "no commit-graph written under $INFO_DIR" >&2
    ls -aR "$INFO_DIR" >&2 || true
    exit 1
fi

# It must be a valid graph for the repo's objects.
if ! git -C "$AUR_DIR" commit-graph verify; then
    echo "commit-graph verify failed" >&2
    exit 1
fi

# A second refresh with the graph present must still succeed (the negotiation
# reads the graph; a broken read would error or change results).
gitaur -Sy
assert_exit 0
assert_stderr_contains "no ref updates"

# Graph still verifies after the second run.
git -C "$AUR_DIR" commit-graph verify || { echo "commit-graph corrupted after 2nd refresh" >&2; exit 1; }

echo "OK: commit-graph written and stable across refreshes"
