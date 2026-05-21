#!/usr/bin/env bash
# Each makepkg invocation tees its stdout+stderr into <worktree>/build.log
# so a user can re-read a failure after the fact. Regression target: when
# resilience landed, a build failure must leave the captured log behind
# (alongside the worktree, which is kept until `gitaur -Sc`).
#
# Assertions: build.log exists, is non-empty, and contains both the
# fixture's sentinel and a makepkg banner — proving both stdout and
# stderr were captured.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-fail-build
assert_exit 1

log="$STATE_DIR/pkgs/test-fail-build/build.log"
[[ -s "$log" ]] || { echo "expected non-empty $log" >&2; ls -la "$STATE_DIR/pkgs/test-fail-build" >&2; _dump >&2; exit 1; }
grep -qF 'GITAUR_FAIL_BUILD_SENTINEL' "$log" || {
    echo "log missing fixture sentinel (stderr capture)" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
}
grep -qF '==>' "$log" || {
    echo "log missing makepkg banner (stdout capture)" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
}
