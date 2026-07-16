#!/usr/bin/env bash
# `-U <file>` passthrough: aurox forwards the local-file install to pacman
# (elevated; the read-only preflight on the -U argv rides along — its
# warning contract is extended/11's business). The staged foreign artifact
# lands in the localdb.
source /work/tests/container/lib.sh
bootstrap; reset_state

pkg=$(ls /srv/foreign-pkgs/test-orphan-foreign-*.pkg.tar.zst 2>/dev/null | head -1)
[[ -n "$pkg" ]] || { echo "no staged test-orphan-foreign artifact" >&2; exit 1; }

aurox -U --noconfirm "$pkg"
assert_exit 0
assert_stderr_contains "sudo pacman -U"
assert_pkg_installed test-orphan-foreign
