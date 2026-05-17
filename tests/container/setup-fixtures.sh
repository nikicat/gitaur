#!/usr/bin/env bash
# Materialize fixture PKGBUILDs into:
#   1. /srv/mock-aur  — a single bare git repo with one `refs/heads/<pkgbase>`
#                      per AUR fixture. This mimics github.com/archlinux/aur's
#                      ref layout exactly so gitaur's mirror logic is exercised.
#   2. /srv/local-repo — a real pacman sync DB built from any fixture marked
#                       as repo='official' (those packages are built once with
#                       makepkg, dropped into the dir, indexed via repo-add).
#
# Each fixture lives under tests/container/fixtures/<pkgbase>/ with at minimum
# a PKGBUILD plus an optional `repo` file ("aur" or "official"). All fixtures
# are designed to build in well under a second — tests assert behavior, not
# realistic compilation work.

set -euo pipefail

FIXTURES_DIR="${FIXTURES_DIR:-/work/tests/container/fixtures}"
MOCK_AUR="${MOCK_AUR:-/srv/mock-aur}"
LOCAL_REPO="${LOCAL_REPO:-/srv/local-repo}"

# ---- mock AUR bare repo ----------------------------------------------------
# One commit per pkgbase, each on a separate branch named after the pkgbase.
# Matches the real github.com/archlinux/aur convention so gitaur's existing
# update_refs / packed-refs code paths run unchanged.
init_mock_aur() {
    git init --bare --quiet "$MOCK_AUR"
    local seed
    seed="$(mktemp -d)"
    pushd "$seed" >/dev/null
    git init --quiet -b main
    git config user.email "test@example.com"
    git config user.name "test"
    git commit --allow-empty -m "seed" --quiet
    git push --quiet "$MOCK_AUR" main
    popd >/dev/null
    rm -rf "$seed"
}

add_aur_pkg() {
    local pkgbase="$1"
    local src="$FIXTURES_DIR/$pkgbase"
    local stage
    stage="$(mktemp -d)"
    cp -r "$src"/* "$stage/"
    pushd "$stage" >/dev/null
    # Real archlinux/aur packages carry both PKGBUILD and .SRCINFO; gitaur's
    # index builder parses the .SRCINFO blob, so synthesize it from PKGBUILD.
    makepkg --printsrcinfo > .SRCINFO
    git init --quiet -b "$pkgbase"
    git config user.email "test@example.com"
    git config user.name "test"
    git add .
    git commit -q -m "$pkgbase: import fixture"
    git push --quiet "$MOCK_AUR" "$pkgbase":"$pkgbase"
    popd >/dev/null
    rm -rf "$stage"
}

# ---- local pacman sync repo ------------------------------------------------
# Each `repo=official` fixture is built with makepkg then registered into
# a sync DB the container's pacman.conf points at. From gitaur's view these
# packages classify as Source::Repo and trigger the pacman-fast-path.
build_and_register_official() {
    local pkgbase="$1"
    local src="$FIXTURES_DIR/$pkgbase"
    local stage
    stage="$(mktemp -d)"
    cp -r "$src"/* "$stage/"
    pushd "$stage" >/dev/null
    makepkg --noconfirm --nodeps --skipinteg
    for pkg in *.pkg.tar.zst; do
        cp "$pkg" "$LOCAL_REPO/"
        repo-add --quiet "$LOCAL_REPO/local-repo.db.tar.gz" "$LOCAL_REPO/$pkg"
    done
    popd >/dev/null
    rm -rf "$stage"
}

main() {
    init_mock_aur
    for dir in "$FIXTURES_DIR"/*/; do
        local pkgbase repo
        pkgbase="$(basename "$dir")"
        repo="aur"
        [[ -f "$dir/repo" ]] && repo="$(cat "$dir/repo")"
        case "$repo" in
            aur)      add_aur_pkg "$pkgbase" ;;
            official) build_and_register_official "$pkgbase" ;;
            *) echo "unknown repo type '$repo' in $dir" >&2; exit 2 ;;
        esac
    done
}

main "$@"
