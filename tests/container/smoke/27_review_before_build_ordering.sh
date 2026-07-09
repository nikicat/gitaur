#!/usr/bin/env bash
# Reviews for every pkgbase in a stratum must complete before the first
# makepkg fires (`-S A B` with two same-stratum siblings). Regression target:
# the old code interleaved review→makepkg per pkgbase, so the user couldn't
# walk through every diff before any build started.
#
# With --noconfirm the prompts collapse to an info-level "auto-proceeding"
# trace line per pkgbase. We bump RUST_LOG so those lines hit stderr, then
# assert both of them appear before the first `==> makepkg …` step.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy

# test-trivial and test-epoch are sibling AUR pkgs with no inter-dependency,
# so they land in the same stratum and exercise the per-stratum ordering.
RUST_LOG=aurox=info aurox -S --noconfirm test-trivial test-epoch
assert_exit 0
assert_pkg_installed test-trivial
assert_pkg_installed test-epoch

# Collect line numbers of phase-1 review markers and phase-2 makepkg steps.
# Both pkgs must have crossed the review gate before the first makepkg ran.
# `|| true` is load-bearing: `set -euo pipefail` would abort the test on a
# grep no-match before our diagnostic dump fires.
trivial_review=$(grep -n 'auto-proceeding.*test-trivial' "$LAST_STDERR" | head -1 | cut -d: -f1 || true)
epoch_review=$(grep -n 'auto-proceeding.*test-epoch' "$LAST_STDERR" | head -1 | cut -d: -f1 || true)
first_makepkg=$(grep -n '==> makepkg' "$LAST_STDERR" | head -1 | cut -d: -f1 || true)

[[ -n "$trivial_review" && -n "$epoch_review" && -n "$first_makepkg" ]] || {
    echo "missing markers: trivial=$trivial_review epoch=$epoch_review makepkg=$first_makepkg" >&2
    _dump >&2
    exit 1
}

(( trivial_review < first_makepkg )) || {
    echo "test-trivial review ($trivial_review) did not precede first makepkg ($first_makepkg)" >&2
    _dump >&2
    exit 1
}
(( epoch_review < first_makepkg )) || {
    echo "test-epoch review ($epoch_review) did not precede first makepkg ($first_makepkg)" >&2
    _dump >&2
    exit 1
}
