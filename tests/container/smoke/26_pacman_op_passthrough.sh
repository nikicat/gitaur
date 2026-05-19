#!/usr/bin/env bash
# `gitaur -S{y,yy,u}*` must forward the *exact* op cluster to pacman: -Su
# stays -Su (no implicit sync DB refresh), -Syu stays -Syu, -Syyu stays
# -Syyu (double-y forces a full re-fetch). Previously the dispatch
# hardcoded `pacman -Syu` whenever `u` was set, which silently turned every
# `-Su` into a sync-DB refresh and collapsed `-Syyu` down to `-Syu`.
#
# We assert against the `--plan` preview line so the test doesn't actually
# run pacman; the line is emitted by the same code path that builds the
# real argv, so a regression in the constructor surfaces here too.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Su --plan
assert_exit 0
assert_stderr_contains "plan: pacman -Su (skipped in --plan mode)"

gitaur -Syu --plan
assert_exit 0
assert_stderr_contains "plan: pacman -Syu (skipped in --plan mode)"

gitaur -Syyu --plan
assert_exit 0
assert_stderr_contains "plan: pacman -Syyu (skipped in --plan mode)"
