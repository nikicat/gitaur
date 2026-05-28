#!/usr/bin/env bash
# Regression for the google-cloud-cli bug: `-Syu` of a foreign-installed
# pkgname in a split pkgbase used to install every sibling makepkg
# packaged from that PKGBUILD.
#
# The path the bug took:
#   * test-syu-split-foreign-cli is installed as FOREIGN at v1.0 (no sync
#     source, not its own AUR pkgbase).
#   * `-Syu`'s upgrade scan classifies it as AsPkgname of pkgbase
#     test-syu-split-foreign (v2.0); the picker row carries
#     `Target::with_hint("test-syu-split-foreign-cli", …)`.
#   * `expand_pkgbase_targets`'s pacman shortcut fires
#     (`pac.is_installed("test-syu-split-foreign-cli") == true`) and
#     passes the spec through unchanged.
#   * USED TO: skip the by_name → selection rewrite, so
#     `Plan.pkgname_selections` had no entry for the pkgbase.
#   * `install_stratum` then had no filter and `pacman -U`'d every
#     sibling produced by makepkg (-cli, -daemon, -desktop), all marked
#     `--asdeps` except the one in `direct_targets`.
#
# Twin to smoke/26 (which exercises the same selection logic on the
# `gaur -S <pkgname>` branch); 26 didn't catch this because that path
# goes through the by_name *rewrite* branch (which already recorded the
# selection). The shortcut at `pac.is_installed || pac.in_sync` was the
# blind spot.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Picker auto-selection: AUR rows are unchecked-by-default under
# `aur_default_select=false` (the safer default), so a vanilla
# `-Syu --noconfirm` would silently skip the AUR upgrade. Enable
# auto-select so the picker actually carries our row through to
# cmd_install — that's what reproduces the original bug.
echo 'aur_default_select = true' >> "$CONFIG_DIR/config.toml"

gaur -Sy

# Seed: install test-syu-split-foreign-cli at v1.0 as FOREIGN (in localdb,
# not in any sync repo, not an AUR pkgbase). Mirrors the user's
# google-cloud-cli-bq starting state.
install_foreign test-syu-split-foreign-cli
assert_pkg_installed test-syu-split-foreign-cli

# Trigger: full `-Syu` cycle. The picker auto-selects the AUR upgrade
# row for test-syu-split-foreign-cli (1.0 → 2.0), which carries an
# explicit hint = test-syu-split-foreign-cli through cmd_install.
gaur -Su --noconfirm
assert_exit 0

# The requested sibling must be upgraded to v2.0 (was foreign 1.0).
assert_pkg_installed test-syu-split-foreign-cli
new_ver=$(pacman -Q test-syu-split-foreign-cli | awk '{print $2}')
[[ "$new_ver" == "2.0-1" ]] || {
    echo "expected test-syu-split-foreign-cli at 2.0-1, got $new_ver" >&2
    _dump >&2
    exit 1
}

# The other siblings must NOT have been pulled in. Without the
# selection-recording fix in the pacman shortcut, all three siblings
# end up in localdb and this assertion fires for the two unrequested
# ones.
assert_pkg_not_installed test-syu-split-foreign-daemon
assert_pkg_not_installed test-syu-split-foreign-desktop
