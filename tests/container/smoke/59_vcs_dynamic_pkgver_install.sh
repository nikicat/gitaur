#!/usr/bin/env bash
# Regression for the `selinux-refpolicy-arch-git` failure: a VCS pkgbase whose
# `pkgver()` overrides the static `.SRCINFO` pkgver must still install. makepkg
# resolves the dynamic version only while extracting sources (rewriting
# `pkgver=` in place), so aurox freezes it two-phase (`--nobuild` then
# `--noextract`) and collects by the real filename. The old path gated
# collection on the stale `.SRCINFO` version (`prep.new_ver`), found no match
# after a clean makepkg exit, and failed with "<pkgbase>: makepkg produced no
# packages".
#
# Fixture `test-vcs-bump-git`: static pkgver=0.r0, pkgver() echoes 0.r99.
# This test FAILS on the pre-fix binary (exit 1, package not installed) and
# passes once the freeze lands.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy

# Plain `-S` install (the --devel gate is only for -Qu upgrade detection; a
# named VCS target builds regardless).
aurox -S --noconfirm test-vcs-bump-git
assert_exit 0
assert_pkg_installed test-vcs-bump-git

# The install must carry the DYNAMIC version pkgver() produced (0.r99), not the
# static .SRCINFO placeholder (0.r0). This is the crux: it proves the build's
# artifact was collected, not silently dropped by a stale-version filter.
installed="$(pacman -Q test-vcs-bump-git 2>/dev/null)"
case "$installed" in
    "test-vcs-bump-git 0.r99-1") ;;
    *)
        echo "expected installed version 0.r99-1 (dynamic pkgver), got: $installed" >&2
        _dump >&2
        exit 1
        ;;
esac

# Re-running the install must not trip makepkg's "a package has already been
# built (use -f)" abort on the artifact from the first run: the frozen-version
# reuse gate recognises it and skips the rebuild.
aurox -S --noconfirm test-vcs-bump-git
assert_exit 0
assert_pkg_installed test-vcs-bump-git
