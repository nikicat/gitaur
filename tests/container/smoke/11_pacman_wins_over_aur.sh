#!/usr/bin/env bash
# When a name lives in BOTH the local sync repo and AUR, pacman wins.
# `repo-base` is in /srv/local-repo; we add an AUR fixture providing it.
source /work/tests/container/lib.sh
bootstrap; reset_state
gaur -Sy

# repo-base must classify Source::Repo even though the AUR index also has it
# (would need a fixture that provides=('repo-base'); add when the fixture lands).
gaur -S --noconfirm repo-base
assert_exit 0
assert_pkg_installed repo-base
# Quick proxy for "didn't go through build pipeline": no worktree was created.
[[ ! -d "$STATE_DIR/pkgs/repo-base" ]] || { echo "AUR build path taken for repo pkg"; exit 1; }
