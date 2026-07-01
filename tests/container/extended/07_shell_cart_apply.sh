#!/usr/bin/env bash
# End-to-end of the interactive shell's cart → approve → apply (REPL phase 3).
#
# The no-arg interactive `gaur` opens the shell. This stages an AUR package into
# the cart, proves the approval gate blocks `apply` until the package is
# approved, approves it without opening a diff, and applies — building and
# installing it through the same `-S` pipeline. The shell is interactive (needs
# a TTY), so the flow is driven by the `shell_cart_e2e` example under a PTY; here
# we build the index it reads and assert the end state.
source /work/tests/container/lib.sh
bootstrap; reset_state

PKGBASE=test-trivial

# Build the on-disk index so the shell can classify $PKGBASE as an AUR package.
# The shell loads the index at startup but never fetches, so `-Sy` must run
# first (exactly what an `upgrade`/`refresh` would do inside the shell later).
gaur -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_cart_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! GITAUR="$GITAUR" "$driver" >"$out" 2>&1; then
    echo "shell cart driver failed (stage/approve/apply did not complete)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_CART_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The staged AUR package was actually built and installed, as an explicit
# (user-requested) install.
assert_pkg_installed "$PKGBASE"
assert_pkg_explicit  "$PKGBASE"

echo "OK — shell staged, approved, and applied $PKGBASE"
