#!/usr/bin/env bash
# Split pkgbase whose pkgdesc lives ONLY in per-pkgname sections (the
# systemd-selinux shape — see fixtures/test-splitdesc). Older gitaur read
# only pkgbase-level pkgdesc, so these showed a blank description everywhere.
# This pins that the per-pkgname description now surfaces in -Ss, -Si, and the
# bare-term picker, and that it's reachable by a regex search over the desc.
source /work/tests/container/lib.sh
bootstrap; reset_state
gaur -Sy
assert_exit 0

# -Ss: the row's description falls back to the pkgname matching the pkgbase
# (the "canonical" member), not a blank line and not the sibling's desc.
gaur -Ss "^test-splitdesc$"
assert_exit 0
assert_stdout_contains "aur/test-splitdesc"
assert_stdout_contains "the main splitdesc package"

# -Si: same headline description for the pkgbase.
gaur -Si test-splitdesc
assert_exit 0
assert_stdout_contains "Description     : the main splitdesc package"

# Regex search now spans per-pkgname descriptions too: a term that only
# appears in the sibling's desc still finds the pkgbase.
gaur -Ss "splitdesc client libraries"
assert_exit 0
assert_stdout_contains "aur/test-splitdesc"

# Bare-term picker (headless = print matches): the AUR row carries its
# canonical description rather than rendering name + version only.
gaur test-splitdesc
assert_exit 0
assert_stdout_contains "aur/test-splitdesc"
assert_stdout_contains "the main splitdesc package"
assert_pkg_not_installed test-splitdesc
