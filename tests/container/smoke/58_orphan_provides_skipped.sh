#!/usr/bin/env bash
# An AUR pkgbase that declares `provides=X` AND `conflicts=X` without
# `replaces=X` is NOT a transparent upgrade for an installed `X` — pacman
# would prompt "Remove X? [y/N]" and abort under `--noconfirm`. Auto-queueing
# the build just reproduces the same failure every loop iteration.
#
# Regression target: dotnet-runtime-7.0 (still installed from an Arch repo
# that has since dropped 7.0) vs dotnet-core-7.0-bin (AUR alternative).
# The picker used to queue the build; we now skip with a structured
# `warn!` in the log and leave the foreign pkg alone.
#
# Assertions:
#   * `aurox -Qu` does NOT list `test-orphan-foreign` (the foreign pkg)
#     as an upgrade row.
#   * The execution log carries a structured warn naming the orphan and
#     the conflicting pkgbase so the user can find it and decide to
#     opt in via `aurox -S <pkgbase>` (which IS allowed).
source /work/tests/container/lib.sh
bootstrap; reset_state

install_foreign test-orphan-foreign
aurox -Sy

aurox -Qu
assert_exit 0

# `-Qu` row would print `test-orphan-aur` for the pkgbase (or
# `test-orphan-foreign` for the foreign side). Neither should appear —
# the orphan stays as-is and the conflicting AUR pkgbase isn't queued.
if grep -qF 'test-orphan-foreign' "$LAST_STDOUT"; then
    echo "expected -Qu to NOT list test-orphan-foreign as an upgrade" >&2
    _dump >&2
    exit 1
fi
if grep -qF 'test-orphan-aur' "$LAST_STDOUT"; then
    echo "expected -Qu to NOT propose test-orphan-aur as the upgrade" >&2
    _dump >&2
    exit 1
fi

log=$(ls -t "$STATE_DIR"/logs/aurox-*.log 2>/dev/null | head -1)
[[ -n "$log" && -s "$log" ]] || {
    echo "expected a non-empty aurox log under $STATE_DIR/logs" >&2
    ls -la "$STATE_DIR/logs" >&2; _dump >&2; exit 1
}

skip=$(grep -F 'provides this pkg but also conflicts' "$log" || true)
[[ -n "$skip" ]] || {
    echo "log missing structured warn about the orphan/conflict skip" >&2
    echo "---- $log ----" >&2; cat "$log" >&2
    exit 1
}
echo "$skip" | grep -qF 'test-orphan-foreign' || {
    echo "skip warn missing installed=test-orphan-foreign" >&2
    echo "  $skip" >&2
    exit 1
}
echo "$skip" | grep -qF 'test-orphan-aur' || {
    echo "skip warn missing pkgbase=test-orphan-aur" >&2
    echo "  $skip" >&2
    exit 1
}
