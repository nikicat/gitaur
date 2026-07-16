#!/usr/bin/env bash
# Materialize fixture PKGBUILDs into:
#   1. /srv/mock-aur    — a single bare git repo with one `refs/heads/<pkgbase>`
#                         per AUR fixture. This mimics github.com/archlinux/aur's
#                         ref layout exactly so aurox's mirror logic is exercised.
#   2. /srv/local-repo  — a real pacman sync DB built from any `repo=official`
#                         fixture (built once with makepkg, dropped into the dir,
#                         indexed via repo-add).
#   3. /srv/foreign-pkgs — staged `.pkg.tar.zst` files for `repo=foreign`
#                         fixtures. NOT registered in any sync DB and NOT mirrored
#                         in /srv/mock-aur — they exist only as artifacts a test
#                         can `pacman -U` to seed a "foreign install" state
#                         (in localdb, but not in any sync source). Models the
#                         dotnet-runtime case: a name that's installed but not
#                         in any current repo and not an AUR pkgbase, so the
#                         resolver must walk `by_provides` to find its upgrade.
#
# Each fixture lives under tests/container/fixtures/<pkgbase>/ with at minimum
# a PKGBUILD plus an optional `repo` file ("aur", "official", or "foreign").
# All fixtures are designed to build in well under a second — tests assert
# behavior, not realistic compilation work.

set -euo pipefail

FIXTURES_DIR="${FIXTURES_DIR:-/work/tests/container/fixtures}"
MOCK_AUR="${MOCK_AUR:-/srv/mock-aur}"
LOCAL_REPO="${LOCAL_REPO:-/srv/local-repo}"
FOREIGN_PKGS="${FOREIGN_PKGS:-/srv/foreign-pkgs}"

# ---- mock AUR bare repo ----------------------------------------------------
# One commit per pkgbase, each on a separate branch named after the pkgbase.
# Matches the real github.com/archlinux/aur convention so aurox's existing
# update_refs / packed-refs code paths run unchanged.
init_mock_aur() {
    git init --bare --quiet "$MOCK_AUR"
    # A dangling HEAD (git's bare-init default is refs/heads/master; we seed
    # `main`) breaks gix's update-refs pass on the first incremental fetch
    # that actually carries updates (extended/18). The real archlinux/aur
    # monorepo never dangles — GitHub always resolves a repo's default
    # branch — so point HEAD at the seed branch to stay faithful.
    git --git-dir="$MOCK_AUR" symbolic-ref HEAD refs/heads/main
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
    # An optional `commit-date` file pins the branch tip's committer time so
    # tests can assert aurox's freshest-commit-first search ordering against
    # known timestamps (otherwise every fixture commits at the same build
    # second and the order is just the pkgbase tie-break). It's metadata for
    # the harness, not part of the package, so drop it before committing the
    # tree. Value is any `git`-parsable date string (e.g. `2020-01-01`).
    local commit_date=""
    if [[ -f "commit-date" ]]; then
        commit_date="$(cat commit-date)"
        rm -f commit-date
    fi
    # Real archlinux/aur packages carry both PKGBUILD and .SRCINFO; aurox's
    # index builder parses the .SRCINFO blob, so synthesize it from PKGBUILD.
    makepkg --printsrcinfo > .SRCINFO
    git init --quiet -b "$pkgbase"
    git config user.email "test@example.com"
    git config user.name "test"
    git add .
    if [[ -n "$commit_date" ]]; then
        GIT_AUTHOR_DATE="$commit_date" GIT_COMMITTER_DATE="$commit_date" \
            git commit -q -m "$pkgbase: import fixture"
    else
        git commit -q -m "$pkgbase: import fixture"
    fi
    git push --quiet "$MOCK_AUR" "$pkgbase":"$pkgbase"
    popd >/dev/null
    rm -rf "$stage"
}

# ---- local pacman sync repo ------------------------------------------------
# Each `repo=official` fixture is built with makepkg then registered into
# a sync DB the container's pacman.conf points at. From aurox's view these
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

# ---- foreign-install artifact stage ---------------------------------------
# Build the PKGBUILD but DO NOT register in any sync repo and DO NOT push to
# the mock AUR. Just drop the .pkg.tar.zst into /srv/foreign-pkgs/ where a
# test can `pacman -U` it to seed a foreign-install state (in localdb, no
# upstream source). See header comment for why this models the dotnet case.
build_and_stage_foreign() {
    local pkgbase="$1"
    local src="$FIXTURES_DIR/$pkgbase"
    local stage
    stage="$(mktemp -d)"
    cp -r "$src"/* "$stage/"
    pushd "$stage" >/dev/null
    makepkg --noconfirm --nodeps --skipinteg
    cp ./*.pkg.tar.zst "$FOREIGN_PKGS/"
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
            foreign)  build_and_stage_foreign "$pkgbase" ;;
            *) echo "unknown repo type '$repo' in $dir" >&2; exit 2 ;;
        esac
    done
}

main "$@"
