#!/usr/bin/env bash
# `aurox -S <installed-pkg>` short-circuits to "nothing to do".
source /work/tests/container/lib.sh
bootstrap; reset_state

sudo pacman -S --noconfirm repo-base >/dev/null
aurox -S --noconfirm repo-base
assert_exit 0
assert_stderr_contains "nothing to do"
