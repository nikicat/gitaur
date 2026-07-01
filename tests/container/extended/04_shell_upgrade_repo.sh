#!/usr/bin/env bash
# End-to-end of the shell's `upgrade` procedure for a repo package (REPL phase 4).
#
# No-arg `gaur` opens the shell. `upgrade` refreshes + seeds the pending repo
# upgrade into the cart (auto-approved); `apply` renders the cost-overlay
# change-set preview, confirms, and runs the partial `pacman -Syu`. The shell is
# interactive (needs a TTY), so the flow is driven by the `shell_upgrade_e2e`
# example under a PTY; here we stage an installed-but-outdated repo package and
# confirm the upgrade landed.
#
# The driver also folds in the synced-db size guard the retired
# `05_loop_size_from_synced_db` test covered (the preview total must not be
# `0 B`).
source /work/tests/container/lib.sh
bootstrap
reset_state

# Seed an outdated install: build loop-repo 1.0 from the fixture (the baked
# local-repo carries 2.0) and install it, so `upgrade` finds a repo upgrade.
work="$(mktemp -d)"
cp /work/tests/container/fixtures/loop-repo/PKGBUILD "$work/"
sed -i 's/^pkgver=.*/pkgver=1.0/' "$work/PKGBUILD"
( cd "$work" && makepkg --noconfirm --nodeps --skipinteg )
sudo pacman -U --noconfirm "$work"/loop-repo-1.0-*.pkg.tar.zst
assert_pkg_installed loop-repo
pacman -Qi loop-repo | grep -q 'Version *: *1.0-1' || {
    echo "seed install is not 1.0" >&2; pacman -Qi loop-repo | grep Version >&2; exit 1
}

# Drive the shell upgrade flow under a PTY.
driver="$EXAMPLES_DIR/shell_upgrade_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! GITAUR="$GITAUR" "$driver" >"$out" 2>&1; then
    echo "shell upgrade driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_UPGRADE_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The upgrade must have actually moved localdb to 2.0.
pacman -Qi loop-repo | grep -q 'Version *: *2.0-1' || {
    echo "shell upgrade did not move loop-repo to 2.0" >&2
    pacman -Qi loop-repo | grep Version >&2
    cat "$out" >&2
    exit 1
}

echo "OK — shell upgrade staged → previewed → applied, loop-repo now 2.0"
