#!/usr/bin/env bash
# End-to-end of the sysupgrade preflight — the libjpeg-turbo shape:
#
#   :: installing libjpeg-turbo (3.2.0-2) breaks dependency 'libjpeg'
#      required by ioquake3-git
#
# pacman only reports this *after* the user confirmed the transaction and
# typed the sudo password. The preflight (a read-only libalpm trans_prepare of
# the staged `-Su`) must surface it at `upgrade`/`show` time and gate `apply`
# before the confirm + sudo — and, since the AUR's current PKGBUILD of the
# dependent no longer needs the broken virtual, offer/schedule the rebuild
# that resolves it (installed ahead of the `pacman -Syu` lane).
#
# Fixture wiring (see the fixtures' header comments):
#   * test-jpeg-provider  — local-repo at 2.0, NO provides. Seeded below at a
#     locally-built 1.0 + `provides=('test-libjpeg')`, so 1.0→2.0 drops the
#     virtual.
#   * test-breaks-dep-old — foreign 1.0 artifact, depends on 'test-libjpeg'.
#   * test-breaks-dep     — AUR 2.0, depends on the concrete provider.
source /work/tests/container/lib.sh
bootstrap
reset_state

# Seed the provider at 1.0 *with* the virtual provides (the fixture's baked
# 2.0 has none — that drop is the breakage under test).
work="$(mktemp -d)"
cp /work/tests/container/fixtures/test-jpeg-provider/PKGBUILD "$work/"
sed -i 's/^pkgver=.*/pkgver=1.0/' "$work/PKGBUILD"
printf "provides=('test-libjpeg')\n" >> "$work/PKGBUILD"
( cd "$work" && makepkg --noconfirm --nodeps --skipinteg )
sudo pacman -U --noconfirm "$work"/test-jpeg-provider-1.0-*.pkg.tar.zst
assert_pkg_installed test-jpeg-provider
pacman -Qi test-jpeg-provider | grep -q 'Version *: *1.0-1' || {
    echo "seed install is not 1.0" >&2
    pacman -Qi test-jpeg-provider | grep Version >&2
    exit 1
}

# The dependent, installed as a foreign package (in localdb, in no sync repo)
# — its 'test-libjpeg' dep is satisfied by the 1.0 provider we just seeded.
install_foreign test-breaks-dep
assert_pkg_installed test-breaks-dep

# Sanity: the un-preflighted upgrade really is doomed — pacman's own resolver
# must refuse it (`--print` keeps this read-only and sudo-free). If this
# passes, the fixture wiring regressed and the preflight has nothing to catch.
if pacman -Sup >/dev/null 2>&1; then
    echo "fixture regression: plain 'pacman -Su' would succeed, nothing to preflight" >&2
    exit 1
fi

# Drive the whole flow — preview note, warning + hint, gated apply, rebuild —
# under a PTY.
driver="$EXAMPLES_DIR/shell_preflight_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "sysupgrade preflight driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_PREFLIGHT_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The transaction landed: provider upgraded, dependent rebuilt from the AUR.
pacman -Qi test-jpeg-provider | grep -q 'Version *: *2.0-1' || {
    echo "repo upgrade did not move test-jpeg-provider to 2.0" >&2
    pacman -Qi test-jpeg-provider | grep Version >&2
    cat "$out" >&2
    exit 1
}
pacman -Qi test-breaks-dep | grep -q 'Version *: *2.0-1' || {
    echo "blocker rebuild did not move test-breaks-dep to 2.0" >&2
    pacman -Qi test-breaks-dep | grep Version >&2
    cat "$out" >&2
    exit 1
}

# The structured preflight events must be in the execution log too (the same
# contract smoke/57 pins for the -U lane).
log=$(ls -t "$STATE_DIR"/logs/aurox-*.log 2>/dev/null | head -1)
[[ -n "$log" ]] && grep -qF 'preflight: unsatisfied dep' "$log" || {
    echo "log missing the structured sysupgrade preflight event" >&2
    [[ -n "$log" ]] && { echo "---- $log ----" >&2; cat "$log" >&2; }
    exit 1
}

echo "OK — preflight warned, gated apply pre-sudo, and the staged rebuild unblocked the upgrade"
