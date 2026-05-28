#!/usr/bin/env bash
# Bare `gitaur` is the yay parity shortcut for `-Syu` (refresh + upgrade).
#
# The distinguishing marker for the upgrade branch (vs. a plain `-Sy`) is
# the `pacman -Syu` invocation that follows the picker: `pacman::invoke`
# prints `:: about to elevate via sudo:` immediately before exec'ing the
# pacman command. A bare `-Sy` refresh never calls pacman, so that line
# only appears when dispatch routed through the upgrade branch. If the
# container has no pending upgrades the dispatch prints `:: nothing to
# do` instead — we accept either.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur --noconfirm
assert_exit 0
grep -qE 'about to elevate via sudo|nothing to do' "$LAST_STDERR" || {
    echo "expected upgrade-branch marker (sudo elevation or 'nothing to do') in stderr" >&2
    _dump >&2
    exit 1
}
