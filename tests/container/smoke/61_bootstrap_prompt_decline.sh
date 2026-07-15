#!/usr/bin/env bash
# The first `-Sy` announces the one-time clone cost and asks; a piped `n`
# declines (exit 0 — a choice, not a failure), leaves no mirror behind, and
# repo installs keep working. A later plain `-Sy` (EOF ⇒ the yes default)
# still bootstraps — a decline isn't sticky.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox_input "n" -Sy
assert_exit 0
assert_stderr_contains "first-time AUR setup"
assert_stderr_contains "AUR setup skipped"
[[ ! -e "$STATE_DIR/aur" ]] || { echo "mirror must not exist after decline" >&2; _dump >&2; exit 1; }

# Repo-only life goes on after the decline.
aurox -S --noconfirm repo-base
assert_exit 0
assert_pkg_installed repo-base

# EOF on stdin takes the yes default — scripts (and the rest of this suite)
# keep bootstrapping with a bare `aurox -Sy`.
aurox -Sy
assert_exit 0
[[ -d "$STATE_DIR/aur" ]] || { echo "expected mirror after accepted -Sy" >&2; _dump >&2; exit 1; }
aurox -Ss "^test-trivial$"
assert_exit 0
assert_stdout_contains "aur/test-trivial"
