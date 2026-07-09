#!/usr/bin/env bash
# Direct target list mixes a repo pkg and an AUR pkg. Both should be marked
# Explicit; aurox shows both lists before its single confirmation.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm repo-base test-trivial
assert_exit 0
assert_pkg_installed repo-base
assert_pkg_explicit repo-base
assert_pkg_installed test-trivial
assert_pkg_explicit test-trivial
