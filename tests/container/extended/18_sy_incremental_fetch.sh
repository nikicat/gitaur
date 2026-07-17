#!/usr/bin/env bash
# `-Sy` is incremental: a no-change refresh reports "no ref updates", and
# after exactly one branch moves on the mirror the next `-Sy` fetches just
# that delta ("1 ref(s) updated") and the index serves the new version —
# no re-clone, no full rebuild.
#
# The mock AUR is a bare repo with one branch per pkgbase (root-owned, so
# the branch bump goes through sudo; safe.directory entries let git cross
# the ownership boundary in both directions).
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

aurox -Sy
assert_exit 0
assert_stderr_contains "no ref updates"

# Bump test-trivial 1.0 → 1.1 on its mock-AUR branch. From a neutral cwd:
# /work may be a *linked git worktree* on the host, whose `.git` is a
# gitdir-pointer file that dangles inside the container — git then dies at
# repo discovery ("fatal: not a git repository: (null)") for every command
# run from /work, even `git config --global`.
work=$(mktemp -d)
cd "$work"
git config --global --add safe.directory '*'
sudo git config --global --add safe.directory '*'
git clone -q -b test-trivial "file://$MOCK_AUR" "$work/pkg"
sed -i 's/^pkgver=1.0$/pkgver=1.1/' "$work/pkg/PKGBUILD"
sed -i 's/pkgver = 1.0/pkgver = 1.1/' "$work/pkg/.SRCINFO"
git -C "$work/pkg" -c user.email=t@example.com -c user.name=t \
    commit -qam 'bump to 1.1'
sudo git -C "$MOCK_AUR" fetch -q "$work/pkg" test-trivial:test-trivial

aurox -Sy
assert_exit 0
assert_stderr_contains "1 ref(s) updated"

aurox -Ss '^test-trivial$'
assert_exit 0
assert_stdout_contains "1.1-1"
