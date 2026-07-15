#!/usr/bin/env bash
# A dependency that resolves nowhere (not a sync-DB name, not an AUR
# pkgbase/pkgname, not a provides) must fail resolution cleanly, naming
# the missing spec AND the package that declared it — the resolver's
# Source::Missing bucket carrying its Requirer into the unknown-target(s)
# error. smoke/10 pins the unknown *target*; this pins the unknown
# *dependency* found mid-walk.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-missing-dep
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit"; _dump; exit 1; }
assert_stderr_contains "unknown target"
assert_stderr_contains "no-such-package-xyz (required by test-missing-dep)"
assert_pkg_not_installed test-missing-dep
