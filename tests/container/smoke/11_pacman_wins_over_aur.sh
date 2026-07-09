#!/usr/bin/env bash
# When a name lives in BOTH the local sync repo and the AUR, pacman wins.
# `repo-base` (1.0-1) is in /srv/local-repo; the AUR index carries
# test-provides-repo-base with provides=('repo-base=9.0') — a virtual version
# far ahead of the repo's, so a resolver that consulted the AUR first (or
# preferred the newer virtual version) would route to the provider instead.
source /work/tests/container/lib.sh
bootstrap; reset_state
gaur -Sy

# repo-base must classify Source::Repo even though the AUR index also
# resolves the name through the provider's provides=.
gaur -S --noconfirm repo-base
assert_exit 0
assert_pkg_installed repo-base
# It's the sync repo's build, not something makepkg produced at 9.0.
pacman -Qi repo-base | grep -q 'Version *: *1.0-1' || {
    echo "installed repo-base is not the sync-repo 1.0-1 build" >&2
    pacman -Qi repo-base | grep Version >&2
    exit 1
}
# The AUR provider was never resolved, built, or installed.
assert_pkg_not_installed test-provides-repo-base
# Quick proxy for "didn't go through build pipeline": no worktree was created.
[[ ! -d "$STATE_DIR/pkgs/repo-base" ]] || { echo "AUR build path taken for repo pkg"; exit 1; }
[[ ! -d "$STATE_DIR/pkgs/test-provides-repo-base" ]] || { echo "AUR provider built for repo pkg"; exit 1; }
