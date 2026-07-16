#!/usr/bin/env bash
# Search annotates matches the row itself can't show. `test-provides-virt`
# declares `provides=('myvirt=2.5')`, so a `myvirt` query is an exact
# provides hit: the bare-term table must flag the row with `[provides
# myvirt]`. This also pins two ranking decisions: the constraint (`=2.5`) is
# stripped before classifying (else nothing is "exact"), and the annotation
# names the tier-earning site even though the fixture's description happens
# to contain "myvirt" too (exact-provides outranks a description match).
#
# `-Ss` stays byte-parity with pacman — no annotation there, ever.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

# Bare-term listing (headless → print ranked matches, install nothing).
aurox myvirt
assert_exit 0
assert_stdout_contains "test-provides-virt"
assert_stdout_contains "[provides myvirt]"

# The plain -Ss surface keeps pacman's exact `repo/name` format: same match,
# no annotation.
aurox -Ss myvirt
assert_exit 0
assert_stdout_contains "aur/test-provides-virt"
assert_stdout_not_contains "[provides"

# Listing is read-only — nothing built or installed.
assert_pkg_not_installed test-provides-virt
