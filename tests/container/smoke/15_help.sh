#!/usr/bin/env bash
# `--help` and `-h` produce the same output and exit 0.
source /work/tests/container/lib.sh

aurox --help
assert_exit 0
assert_stdout_contains "AUROX-OWNED OPERATIONS"

aurox -Sh
assert_exit 0
assert_stdout_contains "AUROX-OWNED OPERATIONS"
