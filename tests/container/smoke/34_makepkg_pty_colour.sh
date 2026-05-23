#!/usr/bin/env bash
# Regression: gitaur runs makepkg under a pty so makepkg's `[[ -t 2 ]]` check
# passes and its `==>` banners stay coloured. Before that change makepkg saw
# a plain pipe and dropped colour entirely.
#
# We can't sniff what the user's terminal actually rendered, but the same
# bytes are tee'd into `<worktree>/build.log` — so an ANSI CSI introducer
# (`\e[`) showing up there proves makepkg emitted SGR codes, i.e. that its
# isatty check passed under our pty. Under the old piped-stdio setup this
# file would be plain ASCII.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-trivial
assert_exit 0

log="$STATE_DIR/pkgs/test-trivial/build.log"
[[ -s "$log" ]] || { echo "expected non-empty $log" >&2; _dump >&2; exit 1; }

# grep -P for `\x1b[` (ESC + CSI). Plain-text `[` would be a false positive,
# so anchoring on the escape byte is what makes this test specific.
if ! grep -qP '\x1b\[' "$log"; then
    echo "build.log missing ANSI escape sequences — pty colour passthrough regressed" >&2
    echo "---- $log ----" >&2
    cat "$log" >&2
    exit 1
fi
