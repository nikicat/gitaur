#!/usr/bin/env bash
# Per-pkgbase failure isolation: when one pkgbase's makepkg fails the run
# must not abort. Independent pkgbases in the same stratum keep building
# and get installed, gitaur exits non-zero, and the failed pkgbase is
# named in the summary.
#
# Pairs test-trivial (succeeds) with test-fail-build (build() returns 1)
# — both AUR, no inter-dep so they share a stratum, so resilience here is
# strictly about isolating the failure (no cascade involved).
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-trivial test-fail-build
assert_exit 1
assert_pkg_installed     test-trivial
assert_pkg_not_installed test-fail-build
assert_stderr_contains   'test-fail-build: build failed'
assert_stderr_contains   'failed test-fail-build'
