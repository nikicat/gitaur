#!/usr/bin/env bash
# `--asdeps` flips Install Reason for direct targets.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -S --noconfirm --asdeps repo-base
assert_exit 0
assert_pkg_installed repo-base
assert_pkg_asdep repo-base
