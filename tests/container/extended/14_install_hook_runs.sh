#!/usr/bin/env bash
# An AUR package carrying an `install=` scriptlet: the final `pacman -U`
# must honor it — test-install-hook's post_install touches
# /var/lib/test-install-hook-ran, so the marker existing proves the hook
# survived the build → tarball → install pipeline (makepkg packs the
# .install file into the artifact; nothing in aurox needs to carry it).
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

aurox -S --noconfirm test-install-hook
assert_exit 0
assert_pkg_installed test-install-hook
[[ -f /var/lib/test-install-hook-ran ]] || { echo "post_install hook did not run"; _dump; exit 1; }
