#!/usr/bin/env bash
# `-S <virtual>` where exactly one pkgname of a split pkgbase declares
# `provides=<virtual>`. yay/paru install only that one pkgname (because the
# AUR-RPC tells them which pkgname declared the provides). aurox must match:
#   * the providing pkgname is built, installed, and marked Explicit;
#   * the sibling pkgnames are NOT built or installed (makepkg --pkg= +
#     install-side file filter both kick in).
#
# Regression target: the bisq case — pkgbase `bisq`, pkgnames `bisq-desktop`
# / `bisq-cli` / `bisq-daemon`, where `bisq-desktop` provides `bisq`. Before
# per-pkgname provides attribution, `-S bisq` would build & install all three.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm scoped-virt
assert_exit 0

# Only the providing pkgname is installed, and as Explicit (the user named it
# via its virtual). The other two siblings must be absent — proving that
# `makepkg --pkg=test-provides-scoped-main` skipped packaging them and the
# install-side filter skipped any leftover files.
assert_pkg_installed test-provides-scoped-main
assert_pkg_explicit  test-provides-scoped-main
assert_pkg_not_installed test-provides-scoped-cli
assert_pkg_not_installed test-provides-scoped-daemon
