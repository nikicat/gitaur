#!/usr/bin/env bash
# `gitaur -S <installed-pkg>` short-circuits to "nothing to do".
source /work/tests/container/lib.sh
bootstrap; reset_state

sudo pacman -S --noconfirm repo-base >/dev/null
gitaur -S --noconfirm repo-base
assert_exit 0
assert_stderr_contains "nothing to do"
