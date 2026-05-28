#!/usr/bin/env bash
# `gaur -Qu` must NOT list VCS pkgbases (`-git`/`-svn`/`-hg`/`-bzr`)
# unless `--devel` is on. Their on-disk pkgver is whatever `pkgver()`
# returned at last build — without `--devel` the upgrade query has no
# way to know upstream has moved, so silently including them would
# produce false positives. Without `--devel`, the only path that can
# surface a VCS pkg is when its archived pkgver in the AUR index has
# advanced past the installed one (rare and bytewise — not what users
# expect `-Qu` to drive).
#
# Listed in `extended/.scope` as `vcs_pkg_skipped_without_devel.sh`.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy

# Build & install the VCS fixture via the regular -S path. Once
# installed, it's a foreign pkg from pacman's view (the AUR isn't a
# sync DB), so it's eligible for `aur_upgrades`'s `pac.foreign()` scan.
gaur -S --noconfirm test-vcs-git
assert_exit 0
assert_pkg_installed test-vcs-git

# Query upgrades without --devel. The table is empty when no upgrades
# exist; either no table prints at all, or it prints without
# mentioning test-vcs-git.
gaur -Qu
assert_exit 0

# upgrade_table writes to stderr (see ui::tables::upgrade_table).
# test-vcs-git must not appear there.
if grep -qF 'test-vcs-git' "$LAST_STDERR"; then
    echo "REGRESSION: test-vcs-git surfaced in -Qu without --devel." >&2
    echo "Upgrade table contained the VCS pkg even though devel-mode is off." >&2
    _dump >&2
    exit 1
fi
