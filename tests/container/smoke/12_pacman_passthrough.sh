#!/usr/bin/env bash
# `gaur -Qi <pkg>` is forwarded to pacman verbatim by the pre-scanner.
source /work/tests/container/lib.sh
bootstrap; reset_state
sudo pacman -S --noconfirm repo-base >/dev/null

gaur -Qi repo-base
assert_exit 0
assert_stdout_contains "Name            : repo-base"
