#!/usr/bin/env bash
# Split pkgbase whose pkgdesc lives ONLY in per-pkgname sections (the
# systemd-selinux shape — see fixtures/test-splitdesc). Older aurox read
# only pkgbase-level pkgdesc, so these showed a blank description everywhere.
# This pins that the per-pkgname description now surfaces in -Ss, -Si, and the
# bare-term listing, and that it's reachable by a regex search over the desc.
source /work/tests/container/lib.sh
bootstrap; reset_state
aurox -Sy
assert_exit 0

# -Ss: the row's description falls back to the pkgname matching the pkgbase
# (the "canonical" member), not a blank line and not the sibling's desc.
aurox -Ss "^test-splitdesc$"
assert_exit 0
assert_stdout_contains "aur/test-splitdesc"
assert_stdout_contains "the main splitdesc package"

# -Si: same headline description for the pkgbase.
aurox -Si test-splitdesc
assert_exit 0
assert_stdout_contains "Description     : the main splitdesc package"

# Regex search now spans per-pkgname descriptions too: a term that only
# appears in the sibling's desc still finds the pkgbase.
aurox -Ss "splitdesc client libraries"
assert_exit 0
assert_stdout_contains "aur/test-splitdesc"

# Bare-term listing (headless = print matches): the aligned table renders repo +
# name as separate columns, and the AUR row carries its canonical description
# rather than rendering name + version only.
aurox test-splitdesc
assert_exit 0
assert_stdout_contains "test-splitdesc"
assert_stdout_contains "the main splitdesc package"
assert_pkg_not_installed test-splitdesc
