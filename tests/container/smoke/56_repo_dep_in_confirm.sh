#!/usr/bin/env bash
# A repo package that pulls in a not-yet-installed repo dependency must
# disclose that dep in the plan and gate the install behind the
# "Proceed with installation?" prompt — the dep is no longer silently pulled
# in by `pacman -S`. The prompt is skipped only when the plan installs exactly
# what the user named (no new deps). Companion to 51, for the repo→repo edge.
source /work/tests/container/lib.sh
bootstrap; reset_state

# aurox's own confirm prints "Proceed…" at the start of a line on stdout;
# pacman uses the same text prefixed with ":: ". Anchor to "^Proceed" so we
# test aurox's prompt, not pacman's.
aurox_prompted() { grep -qE '^Proceed with installation\?' "$LAST_STDOUT"; }

# --- decline: plan prompt fires, dep disclosed, nothing installed ---------
# repo-with-dep depends on repo-helper-lib (in the sync repo, not installed).
# repo-helper-lib is an unrequested dep → NOT only_requested() → prompt. The
# plan table (stderr) must name the dep under "Repo dependencies". Answer "n":
# the run aborts before installing anything.
aurox_input $'n\n' -S repo-with-dep
assert_exit 1
aurox_prompted || {
    echo "repo pkg with an uninstalled dep should show aurox's plan prompt" >&2
    _dump >&2
    exit 1
}
assert_stderr_contains "Repo dependencies"
assert_stderr_contains "repo-helper-lib"
assert_pkg_not_installed repo-with-dep
assert_pkg_not_installed repo-helper-lib

# --- accept (--noconfirm): target explicit, dep installed as --asdeps ------
reset_state
aurox -S --noconfirm repo-with-dep
assert_exit 0
assert_pkg_installed repo-with-dep
assert_pkg_explicit  repo-with-dep
assert_pkg_installed repo-helper-lib
assert_pkg_asdep     repo-helper-lib

# --- dep already installed: no new deps → no plan prompt -------------------
# With repo-helper-lib already present, the plan installs exactly what the
# user named → only_requested() → no "Proceed?" prompt. Empty stdin (EOF=yes)
# clears the sudo gate so the install still completes.
reset_state
sudo pacman -S --noconfirm repo-helper-lib >/dev/null
aurox_input "" -S repo-with-dep
assert_exit 0
if aurox_prompted; then
    echo "no new deps (dep already installed) should not show the plan prompt" >&2
    _dump >&2
    exit 1
fi
assert_pkg_installed repo-with-dep
