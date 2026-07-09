#!/usr/bin/env bash
# The plan-level "Proceed with installation?" prompt is conditional: it only
# fires when the plan drags in packages the user didn't name (repo deps or
# AUR makedepends). For an explicit-only plan the table just echoes the
# user's own request, so the prompt is skipped — the sudo "Continue?" gate
# is the sole confirmation. Drives the interactive path (no --noconfirm).
source /work/tests/container/lib.sh
bootstrap; reset_state

# aurox's own confirm prints the prompt at the start of a line; pacman uses
# the identical question text but prefixes it with ":: ". Anchor the match to
# "^Proceed" so we test for aurox's prompt specifically, not pacman's.
aurox_prompted() { grep -qE '^Proceed with installation\?' "$LAST_STDOUT"; }

# --- explicit-only: no plan prompt, straight to the sudo gate -------------
# repo-base resolves to a single named repo pkg → only_requested(). Empty
# stdin = EOF = "yes" default, so the sudo gate auto-accepts and the install
# completes.
aurox_input "" -S repo-base
assert_exit 0
assert_pkg_installed repo-base
assert_pkg_explicit repo-base
# The redundant prompt must be absent...
if aurox_prompted; then
    echo "explicit-only plan should not show aurox's 'Proceed with installation?' prompt" >&2
    _dump >&2
    exit 1
fi
# ...but the sudo escalation gate must still have fired.
assert_stderr_contains "about to elevate"
assert_stdout_contains "Continue?"

# --- unrequested dep present: plan prompt fires ---------------------------
# test-with-makedep is an AUR pkg whose makedep repo-helper-lib lands in
# transitive_repo → NOT only_requested(). Answer "n" to the plan prompt; the
# run must abort before installing anything (the prompt precedes both the
# repo phase and any AUR review).
reset_state
aurox -Sy
aurox_input $'n\n' -S test-with-makedep
assert_exit 1
aurox_prompted || {
    echo "plan with an unrequested dep should show aurox's plan prompt" >&2
    _dump >&2
    exit 1
}
assert_pkg_not_installed test-with-makedep
assert_pkg_not_installed repo-helper-lib
