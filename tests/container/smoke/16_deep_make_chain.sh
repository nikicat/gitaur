#!/usr/bin/env bash
# 3-deep makedep chain (a → b → c). Each build asserts the makedep is in
# localdb at build time, so this only succeeds if aurox installs each
# stratum before launching the next stratum's makepkg.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-make-chain-a
assert_exit 0
assert_pkg_installed test-make-chain-a
assert_pkg_explicit  test-make-chain-a
assert_pkg_installed test-make-chain-b
assert_pkg_asdep     test-make-chain-b
assert_pkg_installed test-make-chain-c
assert_pkg_asdep     test-make-chain-c
