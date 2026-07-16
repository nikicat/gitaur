#!/usr/bin/env bash
# A failing build inside a shell `apply` must not lose the transaction: the
# same-stratum survivor (test-trivial) still builds + installs (smoke/28's
# isolation contract, surfaced in the shell), the fold drops the landed row
# and keeps ONLY the offender (test-fail-build) staged, and the shell is back
# at a live prompt for `drop` + retry — never a restart. Driven by the
# shell_apply_failure_e2e PTY driver; the end state lands here.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_apply_failure_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }
out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell apply-failure driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_APPLY_FAILURE_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The survivor landed as the explicit install the user staged…
assert_pkg_installed test-trivial
assert_pkg_explicit  test-trivial

# …and the failed build never reached localdb.
assert_pkg_not_installed test-fail-build

echo "OK — apply isolated the failure, kept the offender staged, and resumed at the prompt"
