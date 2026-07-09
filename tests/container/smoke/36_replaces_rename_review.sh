#!/usr/bin/env bash
# Case 4 of the counterpart-resolution matrix
# (`docs/ARCHITECTURE.md#resolution-case-matrix`): explicit `replaces=`
# rename across pkgbases. The user has a foreign legacy pkg installed;
# the new AUR pkgbase has a different pkgname but declares
# `replaces=<legacy>`. The counterpart helper must land on Replaces
# (priority 2, between Pkgname and Provides), and the review header
# must annotate `[replaces <legacy>]`.
#
# Unit-tested in `pacman::alpm_db::tests::counterpart_prefers_replaces_over_provides`
# already; this fixture catches the end-to-end path through
# `expand_pkgbase_targets` (which doesn't route by `replaces=`, so the
# trigger must be the new pkgname) and the noconfirm trace.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy

# Seed: foreign install of the legacy pkg. Models the maintainer-renamed
# pkgbase scenario — the user's localdb still carries the old name.
install_foreign test-replaces-rename-legacy
assert_pkg_installed test-replaces-rename-legacy

# Trigger: install the new pkgname. The expander routes via `by_name`
# (the new pkgname is in the index) and the user-typed pkgname becomes
# the hint. The hint isn't installed, so counterpart_for_hint returns
# None; the unhinted walk then finds the legacy via Replaces.
RUST_LOG=aurox=warn,aurox=info aurox -S --noconfirm test-replaces-rename-new
assert_exit 0
assert_pkg_installed test-replaces-rename-new

strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

trace_line=$(strip_ansi "$LAST_STDERR" | grep 'auto-proceeding.*test-replaces-rename-new' | head -1 || true)
[[ -n "$trace_line" ]] || {
    echo "missing auto-proceeding trace for test-replaces-rename-new" >&2
    _dump >&2
    exit 1
}

# Replaces (tier 2) must win — Pkgname tier is empty (new pkgname not
# in localdb) and Provides tier is empty (no provides= declared).
grep -qF 'via=Some(Replaces)' <<<"$trace_line" || {
    echo "expected via=Some(Replaces) in trace — explicit rename should resolve via Replaces" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

grep -qF 'installed=Some(PkgName("test-replaces-rename-legacy"))' <<<"$trace_line" || {
    echo "expected installed=Some(PkgName(\"test-replaces-rename-legacy\")) in trace" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

# Replaces wins outright — neither diagnostic should fire. The unhinted
# walk's Provides tier is empty (entry has no provides=), so the
# multi-match warning has nothing to count. The hint produced no match,
# so `result == unhinted` and divergence is silent by construction.
if strip_ansi "$LAST_STDERR" | grep -q "multiple installed pkgs match this pkgbase's provides"; then
    echo "REGRESSION: multi-match warning fired with no Provides declaration." >&2
    _dump >&2
    exit 1
fi
