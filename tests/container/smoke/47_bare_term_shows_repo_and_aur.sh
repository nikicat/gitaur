#!/usr/bin/env bash
# yay parity: bare `aurox <term>` searches the sync repos AND the AUR index,
# ranking both in one merged list (older aurox only searched the AUR).
# Headless, it degrades to "print matches, install nothing" — so this pins that
# a repo package and an AUR package both appear in that listing for a query
# that matches each.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

# Every fixture's pkgdesc contains "fixture", so the term matches both the
# local sync-repo packages (e.g. repo-base) and AUR pkgs (e.g. test-trivial).
# alpm searches name+desc; aurox searches AUR name+desc+provides.
aurox fixture
assert_exit 0
# The aligned table renders repo + name as separate columns, so assert on the
# names (repo-sourced `repo-base` and AUR-sourced `test-trivial`) plus the
# `local-repo` repo column, rather than the old `repo/name` form.
assert_stdout_contains "local-repo"
assert_stdout_contains "repo-base"
assert_stdout_contains "test-trivial"

# Listing only — nothing built or installed in the non-interactive path.
assert_pkg_not_installed test-trivial
