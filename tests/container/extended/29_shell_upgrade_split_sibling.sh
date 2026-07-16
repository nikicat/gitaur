#!/usr/bin/env bash
# Shell upgrade of one package out of a split PKGBUILD installs ONLY that
# package — the google-cloud-cli regression, ported from the retired
# smoke/44 (which drove the removed `-Syu` picker path).
#
# test-syu-split-foreign-cli is installed at 1.0 with no repo or AUR entry of
# its own; its AUR pkgbase test-syu-split-foreign (2.0) builds three packages
# from one PKGBUILD: -cli, -daemon, -desktop. The user only has -cli, so even
# though makepkg produces all three, `apply` must install just that one — the
# broken path installed every package makepkg produced. Driven by the
# shell_split_sibling_e2e PTY driver.
source /work/tests/container/lib.sh
bootstrap; reset_state

install_foreign test-syu-split-foreign-cli
assert_pkg_installed test-syu-split-foreign-cli

aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_split_sibling_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }
out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell split-sibling driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_SPLIT_SIBLING_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The picked sibling moved to 2.0…
new_ver=$(pacman -Q test-syu-split-foreign-cli | awk '{print $2}')
[[ "$new_ver" == "2.0-1" ]] || {
    echo "expected test-syu-split-foreign-cli at 2.0-1, got $new_ver" >&2
    exit 1
}

# …and the unrequested siblings stayed out.
assert_pkg_not_installed test-syu-split-foreign-daemon
assert_pkg_not_installed test-syu-split-foreign-desktop

echo "OK — shell upgrade + apply landed only the picked split sibling"
