#!/usr/bin/env bash
# yay parity: bare `gaur <term>` searches the sync repos AND the AUR index,
# listing both in one picker (older gitaur only searched the AUR). Headless,
# the picker degrades to "print matches, install nothing" — so this pins that
# a repo package and an AUR package both appear in that listing for a query
# that matches each.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy
assert_exit 0

# Every fixture's pkgdesc contains "fixture", so the term matches both the
# local sync-repo packages (e.g. repo-base) and AUR pkgs (e.g. test-trivial).
# alpm searches name+desc; gitaur searches AUR name+desc+provides.
gaur fixture
assert_exit 0
assert_stdout_contains "local-repo/repo-base"
assert_stdout_contains "aur/test-trivial"

# Listing only — nothing built or installed in the non-interactive path.
assert_pkg_not_installed test-trivial
