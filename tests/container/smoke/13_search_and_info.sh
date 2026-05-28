#!/usr/bin/env bash
# `-Ss <regex>` and `-Si <pkg>` against the AUR index.
source /work/tests/container/lib.sh
bootstrap; reset_state
gaur -Sy

gaur -Ss "^test-trivial$"
assert_exit 0
assert_stdout_contains "aur/test-trivial"

gaur -Si test-trivial
assert_exit 0
assert_stdout_contains "Name            : test-trivial"
