#!/usr/bin/env bash
# `gitaur -S` with no targets and no -y/-u is a usage error.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -S
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit"; _dump; exit 1; }
assert_stderr_contains "no targets"
