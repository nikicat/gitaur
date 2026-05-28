#!/usr/bin/env bash
# Regression for the bisq-cli bug. `gaur -S <pkgname>` of a split pkgbase
# used to install every sibling .pkg.tar.zst, because the by_name
# passthrough in expand_pkgbase_targets recorded no selection and
# install_stratum had nothing to filter on. Only the requested pkgname
# should land in the install transaction.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy
gaur -S --noconfirm test-bisq-shape-cli
assert_exit 0
assert_pkg_installed test-bisq-shape-cli
assert_pkg_explicit  test-bisq-shape-cli
# Siblings produced by the same pkgbase must NOT have ended up installed.
assert_pkg_not_installed test-bisq-shape-daemon
assert_pkg_not_installed test-bisq-shape-desktop
