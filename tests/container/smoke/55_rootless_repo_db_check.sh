#!/usr/bin/env bash
# Rootless official-repo update check.
#
# `aurox -Sy` refreshes the pacman sync databases WITHOUT root (native libalpm
# into aurox's private dbpath), and the resulting upgrade surfaces in
# `aurox -Qu` — even though the *system* db, which only `sudo pacman -Sy` could
# refresh, still shows the old version. That gap is the whole point of the
# feature: check for repo updates without touching root-owned state.
#
# Scenario:
#   * repo-base 1.0-1 is installed from the local sync repo.
#   * repo-base 2.0-1 is published into the local repo AFTER the system db was
#     last synced, so plain `pacman -Qu` still believes 1.0-1 is current.
#   * `aurox -Sy` (rootless) pulls the fresh db into ~/.local/state/aurox/syncdb.
#   * `aurox -Qu` reads THAT db (via open_synced) and reports 1.0-1 -> 2.0-1.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Turn the feature on (suite default is off) and strip the network repos so the
# rootless sync is hermetic — only the file:// local-repo remains, no mirrors.
cat > "$CONFIG_DIR/config.toml" <<EOF
mirror_url = "file://$MOCK_AUR"
check_repo_updates = true
EOF
awk '/^\[/ { keep = ($0 == "[options]" || $0 == "[local-repo]") } keep' \
    /etc/pacman.conf > /tmp/pacman.conf.hermetic
sudo cp /tmp/pacman.conf.hermetic /etc/pacman.conf

# Install repo-base 1.0-1 from the local repo (the image's system db has it).
sudo pacman -S --noconfirm repo-base >/dev/null
assert_pkg_installed repo-base

# Publish repo-base 2.0-1 into the local repo. repo-add only reads .PKGINFO, so
# a metadata-only package is enough to index a newer version.
stage="$(mktemp -d)"
cat > "$stage/.PKGINFO" <<EOF
pkgname = repo-base
pkgver = 2.0-1
pkgdesc = aurox test fixture (bumped)
arch = any
size = 0
EOF
bsdtar -czf "$stage/repo-base-2.0-1-any.pkg.tar.gz" -C "$stage" .PKGINFO
repo-add --quiet "$LOCAL_REPO/local-repo.db.tar.gz" \
    "$stage/repo-base-2.0-1-any.pkg.tar.gz" >/dev/null

# Precondition: the SYSTEM db hasn't been re-synced since 2.0-1 was published,
# so real `pacman -Qu` must NOT see the upgrade. (If it did, the test would be
# proving nothing.)
if pacman -Qu 2>/dev/null | grep -q '^repo-base '; then
    echo "precondition failed: system db already shows the repo-base upgrade" >&2
    exit 1
fi

# The feature: a rootless `-Sy` refreshes the repo db with no sudo elevation.
aurox -Sy
assert_exit 0
assert_stderr_contains "official package databases refreshed"
assert_stderr_not_contains "about to elevate via sudo"
[[ -f "$STATE_DIR/syncdb/sync/local-repo.db" ]] || {
    echo "private sync db was not populated by aurox -Sy" >&2
    _dump >&2
    exit 1
}

# The payoff: `-Qu` reads the rootless-synced db and reports the upgrade that
# the system db can't. (RUST_LOG=error quiets the "foreign pkg not in AUR index"
# notes for base packages — expected, since core/extra were stripped above.)
export RUST_LOG=aurox=error
aurox -Qu
assert_exit 0
assert_stderr_contains "repo-base"
assert_stderr_contains "1.0-1"
assert_stderr_contains "2.0-1"
