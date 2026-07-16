#!/usr/bin/env bash
# `color = "never"` must strip ANSI even on a real TTY — the case smoke/52
# can't reach (its negative control is Auto on a pipe, where console strips
# escapes anyway). We run `-Qu` under script(1) so stdio is a PTY: first a
# control proving Auto-on-TTY colors the repo prefix (bold-blue "aur", same
# hash smoke/52 pins), then the same command under `never` must emit no
# escape byte at all.
#
# Upgrade seed mirrors smoke/52: a foreign-installed pkgname whose pkgbase
# ships newer in the mock AUR, so `-Qu` renders exactly one AUR row.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0
install_foreign test-syu-split-foreign-cli

esc=$'\033'
aur_colored="${esc}[34m${esc}[1maur"

# Control: default (Auto) on a PTY → colored prefix. TERM must be set —
# the container has none, and console treats missing TERM as no-color,
# which would silently invalidate the control.
out_auto=$(mktemp)
script -qec "TERM=xterm $AUROX -Qu" "$out_auto" >/dev/null || {
    echo "-Qu failed under the control PTY run" >&2; cat "$out_auto" >&2; exit 1;
}
grep -qF -- "$aur_colored" "$out_auto" || {
    echo "control failed: Auto-on-TTY should color the aur prefix" >&2
    cat -v "$out_auto" >&2
    exit 1
}

echo 'color = "never"' >> "$CONFIG_DIR/config.toml"
out_never=$(mktemp)
script -qec "TERM=xterm $AUROX -Qu" "$out_never" >/dev/null || {
    echo "-Qu failed under the never PTY run" >&2; cat "$out_never" >&2; exit 1;
}
if grep -qP '\x1b\[' "$out_never"; then
    echo "color = never must strip every ANSI escape, even on a TTY" >&2
    cat -v "$out_never" >&2
    exit 1
fi
# The row itself is still there, just plain.
grep -q 'aur' "$out_never" || { echo "expected the aur row in plain output" >&2; cat "$out_never" >&2; exit 1; }
