#!/usr/bin/env bash
# A `-R` that pacman would refuse must be refused *before* the sudo consent
# prompt. The motivating transcript: `aurox -R python-pathvalidate` asked
# "Continue?", took the sudo password, and only then died on pacman's
# "removing python-pathvalidate breaks dependency … required by electron-cash".
# The remove preflight (invoke.rs preflight_remove → preflight::remove) runs
# libalpm's trans_prepare against the same localdb first, so the doomed
# transaction never reaches the elevation prompt — and the pacman-native
# escape hatches (`-Rc`, `-Rdd`) travel into the simulation and still work.
source /work/tests/container/lib.sh
bootstrap; reset_state

# repo-with-dep depends on repo-helper-lib, so removing the lib alone breaks it.
sudo pacman -S --noconfirm repo-with-dep >/dev/null
assert_pkg_installed repo-helper-lib

# Broken removal: refused pre-sudo with pacman's own phrasing. No input is
# fed — proof the refusal happens before any prompt could read stdin.
aurox -R repo-helper-lib
assert_exit 1
assert_stderr_contains "removing repo-helper-lib breaks dependency 'repo-helper-lib' required by repo-with-dep"
assert_stderr_contains "failed to prepare transaction"
assert_stderr_not_contains "about to elevate"
assert_pkg_installed repo-helper-lib

# The structured preflight event lands in the execution log (the -R sibling
# of smoke 57's conflict event).
log=$(ls -t "$STATE_DIR"/logs/aurox-*.log 2>/dev/null | head -1)
grep -qF 'preflight: removal breaks dependency' "$log" || {
    echo "log missing removal-breaks preflight event" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
}

# A target that is neither an installed package nor a group: refused pre-sudo.
aurox -R no-such-pkg
assert_exit 1
assert_stderr_contains "unknown target(s): no-such-pkg"
assert_stderr_not_contains "about to elevate"

# A clean removal preflights silently and reaches the normal consent prompt.
aurox_input "n" -R repo-with-dep
assert_exit 1
assert_stderr_contains "about to elevate via sudo"
assert_stderr_contains "sudo pacman -R repo-with-dep"
assert_pkg_installed repo-with-dep

# -Rc pulls the dependents into the transaction, so the same removal that was
# refused above preflights clean and cascades through both packages.
aurox --noconfirm -Rc repo-helper-lib
assert_exit 0
assert_pkg_not_installed repo-helper-lib
assert_pkg_not_installed repo-with-dep
