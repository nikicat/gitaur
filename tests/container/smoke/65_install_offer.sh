#!/usr/bin/env bash
# `aurox -S <aur-pkg>` on a never-synced state offers the one-time AUR setup
# inline (TTY only): accept → bootstrap → the same install retries and
# completes. Driven under a PTY by the install_offer_e2e example.
source /work/tests/container/lib.sh
bootstrap; reset_state

driver="$EXAMPLES_DIR/install_offer_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "install-offer driver failed (offer / consent / bootstrap / retry)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'INSTALL_OFFER_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

assert_pkg_installed test-trivial

# The offer must not fire behind a pipe: same clean state, --noconfirm run
# fails fast with the plain hint instead of prompting or cloning.
sudo pacman -R --noconfirm test-trivial >/dev/null
reset_state
aurox -S --noconfirm test-trivial
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit" >&2; _dump >&2; exit 1; }
assert_stderr_contains "no AUR index"
assert_stderr_not_contains "clone the AUR mirror now?"
[[ ! -e "$STATE_DIR/aur" ]] || { echo "non-interactive -S must not clone" >&2; exit 1; }
