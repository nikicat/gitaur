#!/usr/bin/env bash
# A normal command must self-heal when index.bin is from an older gitaur:
# instead of dumping the raw rkyv error and stopping, gitaur prints a notice,
# resyncs the database, then continues and produces results. This is the
# `gitaur blueprint-compiler` report that motivated load_or_resync, exercised
# end-to-end through the real binary (argv → TLS → bare-term search path).
source /work/tests/container/lib.sh
bootstrap; reset_state

# Build a good index, then clobber it with bytes rkyv can't validate — the
# exact failure mode after a FORMAT_VERSION bump (file present, owned by us,
# layout no longer parses).
gitaur -Sy
assert_exit 0
echo "this is not a valid rkyv archive" > "$STATE_DIR/index.bin"

# Bare-term search (no TTY → lists matches, exit 0). Must recover transparently
# and still surface the AUR hit, with a one-line resync notice on stderr.
gitaur test-trivial
assert_exit 0
assert_stdout_contains "aur/test-trivial"
assert_stderr_contains "resyncing"

# Recovery is durable: the rebuilt index loads cleanly next time, with no
# further resync notice.
gitaur test-trivial
assert_exit 0
assert_stdout_contains "aur/test-trivial"
if grep -qF "resyncing" "$LAST_STDERR"; then
    echo "second run resynced again — the rebuilt index should load cleanly" >&2
    _dump >&2
    exit 1
fi
