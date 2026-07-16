#!/usr/bin/env bash
# The shell's `remove` verb stages an uninstall that `apply` runs as the
# `pacman -R` lane behind the sudo gate. (extended/09 pins the adjacent
# refusal — `remove` of a *staged install* converts rather than uninstalls.)
# Driven by the shell_remove_e2e PTY driver: remove → "will remove" block →
# apply → gate → done → cart empty; the localdb end state lands here.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0
aurox -S --noconfirm test-trivial
assert_exit 0
assert_pkg_installed test-trivial

driver="$EXAMPLES_DIR/shell_remove_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }
out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell remove driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_REMOVE_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

assert_pkg_not_installed test-trivial

echo "OK — shell staged the removal and apply ran the pacman -R lane"
