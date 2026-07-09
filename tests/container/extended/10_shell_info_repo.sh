#!/usr/bin/env bash
# Shell `info` source routing: a sync-repo name is described from the sync
# DBs even though the AUR index also resolves it (test-provides-repo-base
# carries provides=('repo-base=9.0') — repo wins the tie; the old index-only
# lookup would print the provider's block). An AUR-only name still comes from
# the index. The shell needs a TTY, so the shell_info_e2e PTY driver runs it.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Build the on-disk index so the AUR half has something to answer from (the
# shell never fetches at startup); the repo half must work regardless.
gaur -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_info_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! GITAUR="$GITAUR" "$driver" >"$out" 2>&1; then
    echo "shell info driver failed (repo block / aur block)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_INFO_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# `info` is read-only: it must not have staged or installed anything.
assert_pkg_not_installed repo-base
assert_pkg_not_installed test-trivial
