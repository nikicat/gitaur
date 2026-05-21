#!/usr/bin/env bash
# Dep-failure cascade: when an AUR makedep fails, every dependent in a
# later stratum is auto-skipped (its makedep wouldn't be in localdb, so
# attempting the build would just produce a confusing missing-dep error).
# Both pkgbases land in the summary — the failed one as "failed", the
# dependent as "skipped (blocked by …)".
#
# Stratum 0: test-fail-build (fails). Stratum 1: test-needs-fail (skipped).
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-needs-fail
assert_exit 1
assert_pkg_not_installed test-fail-build
assert_pkg_not_installed test-needs-fail
assert_stderr_contains   'test-fail-build: build failed'
assert_stderr_contains   'test-needs-fail: skipping (depends on failed/skipped test-fail-build)'
assert_stderr_contains   'skipped test-needs-fail (blocked by test-fail-build)'
