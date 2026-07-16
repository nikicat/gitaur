#!/usr/bin/env bash
# The shell upgrade table's already-built column (ports the retired
# 06_loop_built_tag now that the picker is gone).
#
# When an AUR upgrade candidate's *new*-version artifact already sits in its
# build worktree — the leftover of a build that completed earlier but wasn't
# installed — the staged row must carry the `built` tag: a `pacman -U` would
# reuse the cached `.pkg.tar.zst` instead of rebuilding. The detection is a
# read-only mirror of `prepare_one`'s idempotency check, so this proves the
# real worktree path + artifact filename + index version line up end to end.
# The shell is interactive, so the assertion lives in the shell_built_tag_e2e
# PTY driver; here we stage the state.
source /work/tests/container/lib.sh
bootstrap; reset_state

PKGBASE=test-trivial

# 1. Installed-but-outdated foreign copy at 1.0-1: build the fixture as-is
#    and `pacman -U` it — in localdb, absent from every sync repo, pkgbase in
#    the mock AUR. Exactly the foreign-AUR-upgrade shape.
work="$(mktemp -d)"
cp /work/tests/container/fixtures/$PKGBASE/PKGBUILD "$work/"
( cd "$work" && makepkg --noconfirm --nodeps --skipinteg )
sudo pacman -U --noconfirm "$work"/$PKGBASE-1.0-1-*.pkg.tar.zst
assert_pkg_installed $PKGBASE

# 2. Publish 2.0-1 to the mock AUR (builder-owned bare repo; --no-hardlinks
#    because $TMPDIR and /srv sit on different mounts).
bump="$(mktemp -d)"
git clone --quiet --no-hardlinks --branch "$PKGBASE" "$MOCK_AUR" "$bump"
( cd "$bump"
  sed -i 's/^pkgver=.*/pkgver=2.0/' PKGBUILD
  makepkg --printsrcinfo > .SRCINFO
  git -c user.email=t@t -c user.name=t commit -aqm "$PKGBASE: bump to 2.0"
  git push --quiet origin "$PKGBASE" )

# 3. Index the bump, then pre-place the 2.0-1 artifact in the build worktree.
aurox -Sy
assert_exit 0
wt="$STATE_DIR/pkgs/$PKGBASE"
mkdir -p "$wt"
v2="$(mktemp -d)"
cp "$bump/PKGBUILD" "$v2/"
( cd "$v2" && makepkg --noconfirm --nodeps --skipinteg )
cp "$v2"/$PKGBASE-2.0-1-*.pkg.tar.zst "$wt/"

# 4. Drive the shell: `upgrade test-trivial` renders the staged row with the
#    `built` tag. Render-only — nothing is applied.
driver="$EXAMPLES_DIR/shell_built_tag_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }
out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell built-tag driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_BUILT_TAG_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# Render-only: localdb still carries 1.0.
pacman -Qi $PKGBASE | grep -q 'Version *: *1.0-1' || {
    echo "render-only flow must not have upgraded $PKGBASE" >&2; exit 1
}

echo "OK — shell upgrade table flagged the pre-built $PKGBASE 2.0 candidate as 'built'"
