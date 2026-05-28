#!/usr/bin/env bash
# Case 6 of the counterpart-resolution matrix
# (`docs/ARCHITECTURE.md#resolution-case-matrix`): the new pkgbase
# declares its provides at PKGBUILD-top level (pkgbase-scope in
# .SRCINFO — tier 3b), and the user types the legacy pkgname directly.
# expand routes via `by_provides` to the new pkgbase and derives the
# legacy as the hint; the counterpart helper must land on Provides with
# the legacy pkgname as the resolved counterpart.
#
# Companion to smoke 39 (case 9), which differs only in the spec the
# user types (the pkgbase, no hint). Together they pin both populated
# and unpopulated hint inputs to the same pkgbase-level Provides path.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy

install_foreign test-pkgbase-provides-legacy
assert_pkg_installed test-pkgbase-provides-legacy

# Trigger: type the legacy pkgname. expand_pkgbase_targets sees it in
# by_provides (the new pkgbase declares it), rewrites the spec to the
# new pkgbase, and records `hints[<pkgbase>] = legacy`. prepare_one
# then calls counterpart_with_hint(entry, Some(legacy)).
RUST_LOG=gitaur=warn,gitaur=info gaur -S --noconfirm test-pkgbase-provides-legacy
assert_exit 0
assert_pkg_installed test-pkgbase-provides-new-a
assert_pkg_installed test-pkgbase-provides-new-b

strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

trace_line=$(strip_ansi "$LAST_STDERR" | grep 'auto-proceeding.*test-pkgbase-provides-new' | head -1 || true)
[[ -n "$trace_line" ]] || {
    echo "missing auto-proceeding trace for test-pkgbase-provides-new" >&2
    _dump >&2
    exit 1
}

grep -qF 'via=Some(Provides)' <<<"$trace_line" || {
    echo "expected via=Some(Provides) — pkgbase-level provides must resolve through tier 3" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

grep -qF 'installed=Some(PkgName("test-pkgbase-provides-legacy"))' <<<"$trace_line" || {
    echo "expected installed=Some(PkgName(\"test-pkgbase-provides-legacy\")) in trace" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

# Single legacy installed, single declared provides line — neither
# diagnostic should fire on this clean path.
if strip_ansi "$LAST_STDERR" | grep -q "multiple installed pkgs match this pkgbase's provides"; then
    echo "REGRESSION: multi-match warning fired with only one provided counterpart." >&2
    _dump >&2
    exit 1
fi

if strip_ansi "$LAST_STDERR" | grep -q 'counterpart hint diverged from unhinted lookup'; then
    echo "REGRESSION: divergence warning fired when hint matched the unhinted walk." >&2
    _dump >&2
    exit 1
fi
