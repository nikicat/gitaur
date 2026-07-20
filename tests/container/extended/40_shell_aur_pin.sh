#!/usr/bin/env bash
# End-to-end of the explicit-AUR-pin: picking the AUR row of a name that a sync
# repo also owns must install the AUR package, overriding pacman precedence.
#
# `aurpin` is BOTH a sync package (1.0, from the `aurpin-repo` fixture) and an
# AUR pkgbase (9.0, from the `aurpin` fixture). A bare `aurpin` classifies as
# the sync package (pacman wins). This drives the shell to `add` the AUR *row*
# by number — which pins the choice — and applies; the shell is interactive, so
# the flow runs under a PTY via the `shell_aur_pin_e2e` example. We assert the
# installed `aurpin` is the AUR 9.0 build, the one routing pacman precedence
# alone can't reach.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Build the on-disk index so the shell sees the AUR `aurpin` alongside the sync
# one (the collision). The shell loads the index at startup but never fetches.
aurox -Sy
assert_exit 0

# Sanity: both really collide — sync `aurpin` 1.0 and an AUR `aurpin` entry.
pacman -Si aurpin >/dev/null 2>&1 || { echo "sync aurpin fixture missing (rebuild image?)" >&2; exit 1; }

driver="$EXAMPLES_DIR/shell_aur_pin_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell aur-pin driver failed (pick AUR row / approve / apply)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_AUR_PIN_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The pin won: the AUR build (9.0-1) is installed, not the sync package (1.0-1).
assert_pkg_installed aurpin
pacman -Qi aurpin | grep -q 'Version *: *9.0-1' || {
    echo "installed aurpin is not the AUR 9.0-1 build — pin did not override pacman" >&2
    pacman -Qi aurpin | grep Version >&2
    exit 1
}

echo "OK — the AUR-row pick installed aurpin 9.0 over the sync namesake"
