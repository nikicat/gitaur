#!/usr/bin/env bash
# Bare, non-interactive `gitaur` is a plain `pacman -Syu` passthrough (the
# interactive shell owns the AUR-aware upgrade flow; with no TTY there's
# nobody to drive it).
#
# The distinguishing marker is the elevation `pacman::invoke` prints —
# `:: about to elevate via sudo:` — immediately before exec'ing `pacman
# -Syu`. A plain `-Sy` refresh never calls pacman, so that line only
# appears when dispatch routed to the passthrough. (If pacman finds nothing
# to upgrade it prints its own "nothing to do" — we accept either.)
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur --noconfirm
assert_exit 0
grep -qE 'about to elevate via sudo|nothing to do' "$LAST_STDERR" || {
    echo "expected upgrade-branch marker (sudo elevation or 'nothing to do') in stderr" >&2
    _dump >&2
    exit 1
}
