#!/usr/bin/env bash
# TTY counterpart to 57_pacman_conflict_logged. When gitaur runs on a real
# terminal, `exec_pacman` hands pacman the inherited TTY so it draws its own
# download/transaction progress bars and reads prompts natively — instead of
# teeing pacman's stdout through a pipe (which forces pacman's degraded
# line-by-line output and risks "error: unable to write to pipe (Broken pipe)").
#
# The branch is observable on failure: the piped path copies pacman's output
# into the execution log ("pacman output captured on failure"); the inherited
# path does NOT (the user saw it live). So the same conflict that 57 asserts
# *is* captured off a TTY must here be *absent* on a TTY — that absence is what
# proves the is_terminal() branch was taken. The libalpm pre-flight diagnostic
# is independent of the channel and must still land.
#
# `script(1)` (util-linux) gives gaur a pty for stdin/stdout/stderr; -e returns
# the child's exit code, -q silences its banner. The seed fixture
# (`test-conflict-pre`) is staged as a foreign artifact; the AUR fixture
# (`test-conflict-aur`) carries `conflicts=('test-conflict-pre')`.
source /work/tests/container/lib.sh
bootstrap; reset_state

install_foreign test-conflict-pre
gaur -Sy

# Drive the conflicting install with gaur's stdio on a pty. typescript saved so
# we can also assert the user's exact reported symptom (the broken pipe) is gone.
ts="$(mktemp)"
set +e
script -qec "$GITAUR -S --noconfirm test-conflict-aur" "$ts" >/dev/null 2>&1
rc=$?
set -e
[[ "$rc" == 1 ]] || {
    echo "expected the conflict to fail with exit 1 under a pty, got $rc" >&2
    cat "$ts" >&2
    exit 1
}

# The reported symptom — pacman's stdout pipe breaking — cannot recur on a tty
# (there is no pipe). Assert it directly against what pacman actually printed.
if grep -qiF 'unable to write to pipe' "$ts"; then
    echo "pacman hit a broken pipe under a tty — the piped-stdio bug is back" >&2
    cat "$ts" >&2
    exit 1
fi

log=$(ls -t "$STATE_DIR"/logs/gitaur-*.log 2>/dev/null | head -1)
[[ -n "$log" && -s "$log" ]] || {
    echo "expected a non-empty gitaur log under $STATE_DIR/logs" >&2
    ls -la "$STATE_DIR/logs" 2>&1 >&2
    exit 1
}

# (1) The libalpm pre-flight conflict event is channel-independent — present on
#     a tty exactly as 57 asserts it is off one.
echo "$(grep -F 'preflight: conflict detected' "$log" || true)" | grep -qF 'test-conflict-aur' || {
    echo "log missing the tty-independent pre-flight conflict event" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
}

# (2) The capture-on-failure event is ABSENT: on a tty gitaur lets pacman own
#     the terminal, so there is nothing to tee into the log. This is the inverse
#     of 57 and the proof exec_pacman took its is_terminal() branch.
if grep -qF 'pacman output captured on failure' "$log"; then
    echo "tty run captured pacman output — inherit branch regressed (it should not tee)" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
fi
