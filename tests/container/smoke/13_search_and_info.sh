#!/usr/bin/env bash
# `-Ss <regex>` and `-Si <pkg>` against the AUR index.
source /work/tests/container/lib.sh
bootstrap; reset_state
aurox -Sy

aurox -Ss "^test-trivial$"
assert_exit 0
assert_stdout_contains "aur/test-trivial"

aurox -Si test-trivial
assert_exit 0
assert_stdout_contains "Name            : test-trivial"
