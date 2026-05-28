#!/usr/bin/env bash
# Pkg with one runtime AUR dep and one AUR makedep. The makedep must be
# pre-installed (drives strata); the runtime dep only needs to be in the
# final pacman -U batch.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy
gaur -S --noconfirm test-mixed-dep-make
assert_exit 0
assert_pkg_installed test-mixed-dep-make
assert_pkg_explicit  test-mixed-dep-make
assert_pkg_installed test-trivial          # runtime dep
assert_pkg_asdep     test-trivial
assert_pkg_installed test-make-chain-c     # build-time dep
assert_pkg_asdep     test-make-chain-c
