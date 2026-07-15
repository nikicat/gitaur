#!/usr/bin/env bash
# Without an AUR index, -Ss/-Si degrade to repo-only results (one nudge, not
# an error), exit codes stay pacman-like, and misses say how to enable the AUR.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Ss "^repo-base$"
assert_exit 0
assert_stdout_contains "local-repo/repo-base"
assert_stderr_contains "no AUR index; showing repo matches only"

# No match anywhere: silent stdout, exit 1 (pacman parity).
aurox -Ss "^no-such-pkg-xyz$"
assert_exit 1

# -Si: the repo block prints; an unknown name explains the missing AUR half.
aurox -Si repo-base
assert_exit 0
assert_stdout_contains "Name            : repo-base"
aurox -Si test-trivial
assert_exit 1
assert_stderr_contains "no AUR index"
assert_stderr_contains "aurox -Sy"
