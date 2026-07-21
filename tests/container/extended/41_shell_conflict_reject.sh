#!/usr/bin/env bash
# A declared conflict is rejected at `add`, not at apply. Staging an AUR package
# that declares `conflicts=` a co-staged AUR package (with no matching
# `replaces=`) fails the shell's whole-cart resolve up front and rolls the cart
# back — instead of pacman's prepare failing at apply, after the build. The
# shell is interactive, so the flow runs under a PTY via the
# `shell_conflict_e2e` example. (`test-xconflict-bin` conflicts with
# `test-xconflict`; both are AUR fixtures.)
source /work/tests/container/lib.sh
bootstrap; reset_state

# Build the on-disk index so the shell sees both AUR fixtures. The shell loads
# the index at startup but never fetches.
aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_conflict_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell conflict driver failed (stage base / reject -bin on conflict)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_CONFLICT_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The reject applied nothing — neither package is installed.
assert_pkg_not_installed test-xconflict
assert_pkg_not_installed test-xconflict-bin

echo "OK — a declared conflict was rejected at add; nothing installed"
