#!/usr/bin/env bash
# Ctrl-C during a shell `apply` build bails to the prompt, never out of aurox.
#
# The shell_ctrl_c_e2e PTY driver stages test-sleep-build (its build() prints
# a sentinel then sleeps), applies, and sends the real ^C byte once the
# sentinel shows — SIGINT lands on aurox's foreground process group exactly
# as a terminal would deliver it. aurox catches it, marks the build
# interrupted, keeps the cart for retry, and returns to a live prompt (the
# driver's clean `quit` is the proof). extended/02 pins the same interrupt's
# forward-to-makepkg mechanics on the `-S` path; this pins the shell resume.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_ctrl_c_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }
out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell ctrl-c driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_CTRL_C_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# Nothing installed, and the interrupted build's sleeping child didn't
# survive as an orphan (the SIGINT forward reached makepkg's process group).
assert_pkg_not_installed test-sleep-build
if pgrep -f 'sleep 3137' >/dev/null 2>&1; then
    echo "orphaned build child survived the interrupt:" >&2
    pgrep -af 'sleep 3137' >&2
    exit 1
fi

echo "OK — Ctrl-C interrupted the build, kept the cart, and the shell resumed at the prompt"
