#!/usr/bin/env bash
# The shell's first-launch three-way AUR question, end to end under a PTY:
# Enter (= Later) persists nothing and `refresh` bootstraps without a second
# question; `n` persists `aur = false` and the next launch neither asks nor
# nags. Driven by the shell_bootstrap_{later,decline}_e2e examples.
source /work/tests/container/lib.sh
bootstrap; reset_state

run_driver() {
    local name="$1" marker="$2"
    local driver="$EXAMPLES_DIR/$name"
    [[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }
    local out; out="$(mktemp)"
    if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
        echo "$name failed" >&2
        cat "$out" >&2
        exit 1
    fi
    grep -qF "$marker" "$out" || { echo "$name did not report success" >&2; cat "$out" >&2; exit 1; }
}

# Later: nothing persisted — no `aur` key in config — and the in-session
# `refresh` bootstrapped the mirror.
run_driver shell_bootstrap_later_e2e SHELL_BOOTSTRAP_LATER_E2E_OK
! grep -q '^aur =' "$CONFIG_DIR/config.toml" \
    || { echo "'later' must not write config" >&2; cat "$CONFIG_DIR/config.toml" >&2; exit 1; }
[[ -d "$STATE_DIR/aur" ]] || { echo "in-session refresh should have bootstrapped" >&2; exit 1; }

# Decline: exactly one sparse `aur = false` line lands in config; no clone.
reset_state
run_driver shell_bootstrap_decline_e2e SHELL_BOOTSTRAP_DECLINE_E2E_OK
grep -q '^aur = false' "$CONFIG_DIR/config.toml" \
    || { echo "aur = false not persisted" >&2; cat "$CONFIG_DIR/config.toml" >&2; exit 1; }
[[ ! -e "$STATE_DIR/aur" ]] || { echo "decline must not clone" >&2; exit 1; }
