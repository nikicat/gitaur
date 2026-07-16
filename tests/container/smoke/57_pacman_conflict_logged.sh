#!/usr/bin/env bash
# When `pacman -U` fails on a conflict, the execution log must carry enough
# detail to post-mortem the failure without re-running. The diagnostic chain
# in invoke.rs lands two signals in $STATE_DIR/logs/aurox-*.log:
#   1. libalpm pre-flight: a `preflight: conflict detected` debug event with
#      structured pkg1=/pkg2= fields (we run trans_prepare ourselves so the
#      offending pair is queryable in the log, not buried in pacman's prompt).
#   2. Captured pacman stdout+stderr on failure: ANSI-stripped, includes the
#      "are in conflict" prompt body that pacman wrote to stdout under
#      --noconfirm before exiting 1.
#
# Regression target: AFK user finds the log and needs to identify the
# conflict pair without scrollback (which is gone). The seed fixture
# (`test-conflict-pre`) is staged as a foreign artifact; the AUR fixture
# (`test-conflict-aur`) carries `conflicts=('test-conflict-pre')`.
source /work/tests/container/lib.sh
bootstrap; reset_state

install_foreign test-conflict-pre
aurox -Sy

# Install the AUR pkg whose PKGBUILD conflicts with the seed.
aurox -S --noconfirm test-conflict-aur
assert_exit 1

log=$(ls -t "$STATE_DIR"/logs/aurox-*.log 2>/dev/null | head -1)
[[ -n "$log" && -s "$log" ]] || {
    echo "expected a non-empty aurox log under $STATE_DIR/logs" >&2
    ls -la "$STATE_DIR/logs" 2>&1 >&2
    _dump >&2
    exit 1
}

# (1) Structured pre-flight event with both pkgnames as fields.
preflight=$(grep -F 'preflight: conflict detected' "$log" || true)
[[ -n "$preflight" ]] || {
    echo "log missing pre-flight conflict event" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
}
echo "$preflight" | grep -qF 'test-conflict-aur' || {
    echo "pre-flight event missing pkg1=test-conflict-aur" >&2
    echo "  $preflight" >&2
    exit 1
}
echo "$preflight" | grep -qF 'test-conflict-pre' || {
    echo "pre-flight event missing pkg2=test-conflict-pre" >&2
    echo "  $preflight" >&2
    exit 1
}

# (2) Captured-output event with pacman's stdout prompt body.
grep -qF 'pacman output captured on failure' "$log" || {
    echo "log missing captured-output event" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
}
grep -qF 'are in conflict' "$log" || {
    echo "log missing pacman stdout conflict prompt (stdout capture regressed)" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
}

# (3) ANSI escapes were stripped — no raw ESC[ bytes in the log.
if grep -qaP '\x1b\[' "$log"; then
    echo "log contains raw ANSI escape bytes — strip_ansi_codes regressed" >&2
    grep -anP '\x1b\[' "$log" | head -5 >&2
    exit 1
fi
