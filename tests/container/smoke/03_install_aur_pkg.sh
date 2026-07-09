#!/usr/bin/env bash
# `aurox -S <aur-pkg>` runs full build pipeline against the mock mirror.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

aurox -S --noconfirm test-trivial
assert_exit 0
assert_pkg_installed test-trivial
assert_pkg_explicit test-trivial
