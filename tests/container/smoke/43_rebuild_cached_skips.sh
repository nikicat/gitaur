#!/usr/bin/env bash
# Build idempotency: a second `gaur -S <pkg>` for the same pkgbase at
# the same version must reuse the cached `.pkg.tar.zst` rather than
# rerunning makepkg. The check in `prepare_one` (`src/build.rs`)
# derives "already built" from whether a tarball at exactly `new_ver`
# exists for every required pkgname — purely from on-disk artifacts,
# no sidecar DB. The user-visible signal is the `<pkgbase>: already
# built <ver>` note on stderr; the harder signal is the `.pkg.tar.zst`
# mtime, which makepkg would otherwise overwrite.
#
# Listed in `extended/.scope` as `rebuild_cached_skips.sh`. Promoted to
# smoke because it pins the load-bearing idempotency check that lets
# declined `pacman -U` / interrupted installs be retried cheaply.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy

gaur -S --noconfirm test-trivial
assert_exit 0
assert_pkg_installed test-trivial

pkg_file=$(ls "$STATE_DIR"/pkgs/test-trivial/*.pkg.tar.zst 2>/dev/null | head -1)
[[ -n "$pkg_file" ]] || {
    echo "no built .pkg.tar.zst found under $STATE_DIR/pkgs/test-trivial/" >&2
    ls -la "$STATE_DIR/pkgs/test-trivial/" >&2 || true
    exit 1
}
initial_mtime=$(stat -c '%Y' "$pkg_file")

# Second run — must hit the cached path.
gaur -S --noconfirm test-trivial
assert_exit 0

grep -qF 'test-trivial: already built' "$LAST_STDERR" || {
    echo "expected 'test-trivial: already built …' note on stderr" >&2
    _dump >&2
    exit 1
}

new_mtime=$(stat -c '%Y' "$pkg_file")
[[ "$initial_mtime" == "$new_mtime" ]] || {
    echo "REGRESSION: .pkg.tar.zst mtime changed — makepkg ran on the cached run" >&2
    echo "  before: $initial_mtime  after: $new_mtime" >&2
    echo "  file:   $pkg_file" >&2
    _dump >&2
    exit 1
}
