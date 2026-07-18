#!/usr/bin/env bash
# End-to-end of the launch splash's idle eye-blink: left alone at the first
# prompt, the ox winks "AUROX" in Morse — the eyes go (oo) -> (--) with no
# input. Driven under a PTY by the shell_splash_blink_e2e example. The reverse,
# "a keystroke cancels the blink", is already exercised by every other shell
# driver: they type within the idle window and complete cleanly.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Sync the index so the shell opens straight to the banner (a never-synced AUR
# would ask the first-launch question before the banner instead).
aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_splash_blink_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell splash-blink driver failed (banner shown / eyes never winked shut)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_SPLASH_BLINK_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }
