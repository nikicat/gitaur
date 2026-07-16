#!/usr/bin/env bash
# The shell upgrade's provides-hint plumbing — the dotnet-runtime shape, ported
# from the retired smoke/33 (which drove the removed `-Syu` picker path):
#
#   * test-syu-hint-newer installed foreign at 9.0 (vercmp-newer than the AUR
#     pkgbase's 2.0) — must NOT seed an upgrade row;
#   * test-syu-hint-older installed foreign at 1.0 — `upgrade` seeds it, named
#     by the foreign pkgname (the hint);
#   * pkgbase test-syu-hint-new declares provides=(newer older), newer FIRST.
#
# The hint must reach the counterpart walk: the shell_provides_hint_e2e driver
# asserts the review header reads `[provides test-syu-hint-older]` — unhinted,
# the walk picks the first-declared installed provides (-newer), the original
# wrong-label bug. Then review-approve → apply, and the end state lands here.
source /work/tests/container/lib.sh
bootstrap; reset_state

install_foreign test-syu-hint-newer
assert_pkg_installed test-syu-hint-newer
install_foreign test-syu-hint-older
assert_pkg_installed test-syu-hint-older

aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/shell_provides_hint_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }
out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "shell provides-hint driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_PROVIDES_HINT_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The pkgbase build landed…
assert_pkg_installed test-syu-hint-new
ver=$(pacman -Q test-syu-hint-new | awk '{print $2}')
[[ "$ver" == "2.0-1" ]] || { echo "expected test-syu-hint-new at 2.0-1, got $ver" >&2; exit 1; }

# …and the vercmp-newer foreign was left alone.
ver=$(pacman -Q test-syu-hint-newer | awk '{print $2}')
[[ "$ver" == "9.0-1" ]] || { echo "test-syu-hint-newer must stay at 9.0-1, got $ver" >&2; exit 1; }

# The deep plumbing signal: the hint diverged from the unhinted walk (older
# vs first-declared newer), and the divergence warning is in the session's
# execution log (the file layer records debug+ regardless of console filter).
log=$(ls -t "$STATE_DIR"/logs/aurox-*.log 2>/dev/null | head -1)
[[ -n "$log" ]] && grep -q 'counterpart hint diverged from unhinted lookup' "$log" || {
    echo "log missing the hint-divergence warning — the hint didn't reach the walk" >&2
    [[ -n "$log" ]] && { echo "---- $log ----" >&2; tail -50 "$log" >&2; }
    exit 1
}

echo "OK — shell upgrade seeded the foreign row, the review header carried the hint, and apply landed the pkgbase"
