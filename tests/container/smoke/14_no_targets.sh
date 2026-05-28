#!/usr/bin/env bash
# `gaur -S` with no targets and no -y/-u is a usage error.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -S
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit"; _dump; exit 1; }
assert_stderr_contains "no targets"
