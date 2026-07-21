#!/usr/bin/env bash
# An already-installed OFFICIAL-repo package must not be dragged onto the AUR
# build path when the AUR index happens to carry the same name.
#
# The regression (webp-pixbuf-loader): a user staged the installed
# `extra/webp-pixbuf-loader` (0.2.7-2); at apply the direct-rebuild override
# saw "installed + name known to the AUR" and flipped it to an AUR "upgrade"
# to the unrelated, outdated AUR `webp-pixbuf-loader` (0.0.1-3). The override
# is meant for *foreign* (`-Qm`) installs only — pacman must keep the shared
# name.
#
# Reproduction here uses the existing collision fixtures:
#   * repo-base                (official, 1.0-1, in the local sync DB)
#   * test-provides-repo-base  (AUR, provides=('repo-base=9.0'))
# `test-provides-repo-base` gives the AUR index a `repo-base` entry (via
# provides), so once repo-base is installed the buggy override would rebuild
# the AUR provider. smoke/11 covers the *not-yet-installed* case (Source::Repo);
# this covers the installed case (Source::Installed), which is the untested gap.
source /work/tests/container/lib.sh
bootstrap; reset_state
aurox -Sy   # load the AUR index — test-provides-repo-base's repo-base provide

# First install: repo-base is not yet installed, so it classifies Source::Repo
# and lands via pacman (smoke/11's path).
aurox -S --noconfirm repo-base
assert_exit 0
assert_pkg_installed repo-base

# Re-run with repo-base now INSTALLED. It classifies Source::Installed; the
# override must NOT fire (repo-base is a sync-repo pkg, not foreign), so this
# is a plain "already satisfied" no-op — never an AUR rebuild of the provider.
aurox -S --noconfirm repo-base
assert_exit 0
assert_stderr_contains "nothing to do"

# It's still the sync repo's 1.0-1 build, untouched by any AUR "upgrade".
pacman -Qi repo-base | grep -q 'Version *: *1.0-1' || {
    echo "installed repo-base is not the sync-repo 1.0-1 build" >&2
    pacman -Qi repo-base | grep Version >&2
    exit 1
}
# The AUR provider was never resolved, built, or installed.
assert_pkg_not_installed test-provides-repo-base
[[ ! -d "$STATE_DIR/pkgs/test-provides-repo-base" ]] || {
    echo "AUR provider built for an installed repo pkg (override fired on a non-foreign install)" >&2
    exit 1
}
