#!/usr/bin/env bash
# `aurox -Qu --devel` MUST list every installed VCS pkgbase
# (`-git`/`-svn`/`-hg`/`-bzr`) regardless of vercmp. With --devel the
# user has explicitly asked aurox to treat VCS pkgs as
# always-outdated — `pkgver()` only refreshes when makepkg runs, so the
# current pkgver is presumed stale until proven otherwise by a
# rebuild. Companion to smoke 41 (which pins the no-devel skip).
#
# Listed in `extended/.scope` as `vcs_pkg_picked_up_with_devel.sh`.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy

aurox -S --noconfirm test-vcs-git
assert_exit 0
assert_pkg_installed test-vcs-git

aurox -Qu --devel
assert_exit 0

# upgrade_table writes the rows to stderr. With --devel, test-vcs-git
# must be among them — the installed and AUR versions are equal here
# (we just built from the same PKGBUILD), so this exercises the
# `devel && is_vcs` branch specifically rather than the `is_outdated`
# fall-through.
grep -qF 'test-vcs-git' "$LAST_STDERR" || {
    echo "expected test-vcs-git in the -Qu --devel upgrade table" >&2
    _dump >&2
    exit 1
}
