#!/usr/bin/env bash
# End-to-end of the interactive shell's cart editing: number-follows-the-shown-
# list resolution, `remove`'s refusal to uninstall a staged install, and
# `undo`/`redo`.
#
# The shell is interactive (needs a TTY), so the flow is driven by the
# `shell_undo_e2e` example under a PTY; here we build the index it reads and
# assert nothing was installed (the driver only edits the cart and quits — it
# never `apply`s — so none of `add`/`keep`/`undo`/`redo` may install anything).
source /work/tests/container/lib.sh
bootstrap; reset_state

# Build the on-disk index so the shell can classify test-trivial and test-epoch
# as AUR packages (the shell loads the index at startup but never fetches).
gaur -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_undo_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! GITAUR="$GITAUR" "$driver" >"$out" 2>&1; then
    echo "shell undo driver failed (remove-reject / keep / undo / redo did not complete)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_UNDO_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# Pure cart editing must not install anything — no `apply` was run.
assert_pkg_not_installed test-trivial
assert_pkg_not_installed test-epoch

echo "OK — shell cart editing (remove-reject, undo, redo) works end-to-end"
