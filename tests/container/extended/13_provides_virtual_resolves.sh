#!/usr/bin/env bash
# `-S <virtual>` where nothing is named `myvirt`, but AUR pkgbase
# test-provides-virt declares `provides=('myvirt=2.5')`: the by_provides
# secondary lookup must resolve the target (stripping the `=2.5` off the
# provider-side spec), build the provider, and install it as Explicit —
# the pkgname the user typed is satisfied, not literally installed.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

aurox -S --noconfirm myvirt
assert_exit 0
assert_pkg_installed test-provides-virt
assert_pkg_explicit  test-provides-virt
# The virtual is satisfied, but no package literally named myvirt exists in
# the localdb (`pacman -Qi myvirt` would deceive here — -Q resolves provides
# among installed packages, answering with the provider).
pacman -T 'myvirt=2.5' >/dev/null || { echo "expected myvirt=2.5 satisfied"; _dump; exit 1; }
! pacman -Q | grep -q '^myvirt ' || { echo "expected no literal myvirt package"; _dump; exit 1; }
