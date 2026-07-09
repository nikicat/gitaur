#!/usr/bin/env bash
# Bare `aurox <term>` is the yay parity shortcut for a repo+AUR fuzzy search.
# Interactively it launches the shell REPL seeded with the search; with no TTY
# (the container runs headless) it degrades to "print the ranked matches and
# exit 0" instead of auto-installing every regex hit — an explicit safety call.
# This test pins that behaviour so a future refactor doesn't silently start
# installing matched packages in non-interactive contexts.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

aurox "^test-trivial$"
assert_exit 0
# The aligned table renders repo + name as separate columns (like the upgrade
# table), so the row reads `aur   test-trivial …` rather than `aur/test-trivial`.
assert_stdout_contains "test-trivial"

# Nothing should have been built or installed — the non-interactive path
# only lists matches. Asserts the safety promise above.
assert_pkg_not_installed test-trivial
