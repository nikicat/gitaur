#!/usr/bin/env bash
# End-to-end of the bare-term launch: `aurox <term>` opens the shell seeded with
# that search (no picker — the REPL is the one interactive surface).
#
# The shell is interactive (needs a TTY), so the flow is driven by the
# `shell_search_seed_e2e` example under a PTY; here we build the index it reads
# and assert nothing was installed (the driver only stages by number, then
# quits, so the search launch must not install on its own).
source /work/tests/container/lib.sh
bootstrap; reset_state

# Build the on-disk index so the seeded search can classify test-trivial as an
# AUR package (the shell loads the index at startup but never fetches).
aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_search_seed_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell search-seed driver failed (banner / seeded list / add-by-number)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_SEARCH_SEED_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The launch only searches + stages into the cart; without `apply` nothing is
# built or installed. Pins that the bare-term launch is not an install shortcut.
assert_pkg_not_installed test-trivial
