#!/usr/bin/env bash
# Bare `gaur <term>` is the yay parity shortcut for AUR fuzzy search →
# interactive multi-select → install. With no TTY (the container runs
# headless), the picker degrades to "print matches and exit 0" instead of
# auto-installing every regex hit — an explicit safety call. This test
# pins that behaviour so a future refactor doesn't silently start
# installing matched packages in non-interactive contexts.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy
assert_exit 0

gaur "^test-trivial$"
assert_exit 0
assert_stdout_contains "aur/test-trivial"

# Nothing should have been built or installed — the non-interactive path
# only lists matches. Asserts the safety promise above.
assert_pkg_not_installed test-trivial
