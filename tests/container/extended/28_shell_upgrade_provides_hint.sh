#!/usr/bin/env bash
# Shell upgrade of a package the user knows by a different name — the
# dotnet-runtime story, ported from the retired smoke/33 (which drove the
# removed `-Syu` picker path).
#
# Two installed packages exist in no repo and have no AUR entry of their own
# ("foreign"). Their only upgrade path is AUR package test-syu-hint-new,
# which lists both in provides=, the newer one first:
#   * test-syu-hint-newer at 9.0 — newer than the AUR package's 2.0, so no
#     upgrade may be offered for it;
#   * test-syu-hint-older at 1.0 — outdated, so `upgrade` stages it under
#     that name.
#
# The shell_provides_hint_e2e driver asserts the review header names the
# package the user acted on (`[provides test-syu-hint-older]`); the broken
# lookup ignored that and took the first installed name from the provides
# list (-newer). After review-approve → apply, the system state is checked
# here.
source /work/tests/container/lib.sh
bootstrap; reset_state

install_foreign test-syu-hint-newer
assert_pkg_installed test-syu-hint-newer
install_foreign test-syu-hint-older
assert_pkg_installed test-syu-hint-older

aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_provides_hint_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }
out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell provides-hint driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_PROVIDES_HINT_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The AUR package landed and is marked "Explicitly installed". The user asked
# for it (under the name of the installed package it replaces); marking it
# "installed as a dependency" instead would let a later orphan cleanup
# (pacman -Rns $(pacman -Qtdq)) remove it.
assert_pkg_installed test-syu-hint-new
assert_pkg_explicit  test-syu-hint-new
ver=$(pacman -Q test-syu-hint-new | awk '{print $2}')
[[ "$ver" == "2.0-1" ]] || { echo "expected test-syu-hint-new at 2.0-1, got $ver" >&2; exit 1; }

# …and the already-newer package was left alone.
ver=$(pacman -Q test-syu-hint-newer | awk '{print $2}')
[[ "$ver" == "9.0-1" ]] || { echo "test-syu-hint-newer must stay at 9.0-1, got $ver" >&2; exit 1; }

# Extra internal signal: when the user's chosen name changes the outcome of
# the installed-counterpart lookup, aurox logs a warning about it. That
# warning must be in the session's execution log (the log file records
# debug+ even when console logging is off).
log=$(ls -t "$STATE_DIR"/logs/aurox-*.log 2>/dev/null | head -1)
[[ -n "$log" ]] && grep -q 'counterpart hint diverged from unhinted lookup' "$log" || {
    echo "log missing the hint-divergence warning — the user's name never reached the lookup" >&2
    [[ -n "$log" ]] && { echo "---- $log ----" >&2; tail -50 "$log" >&2; }
    exit 1
}

echo "OK — shell upgrade seeded the foreign row, the review header carried the hint, and apply landed the pkgbase"
