#!/usr/bin/env bash
# Case 9 of the counterpart-resolution matrix
# (`docs/ARCHITECTURE.md#resolution-case-matrix`): same shape as case
# 6 (pkgbase-level provides in the new pkgbase, legacy installed
# foreign), but the user types the PKGBASE rather than the legacy
# pkgname. expand routes via `by_name` directly to the pkgbase entry
# — and because the user didn't name a pkgname, no hint is derived.
# The counterpart helper falls through to the unhinted walk, which
# must still surface the legacy via the pkgbase-level Provides tier.
#
# Companion to smoke 38 (case 6). Reuses the same split-pkg fixture
# `test-pkgbase-provides-new` and the same foreign baseline; the only
# axis under test here is the hint=None code path.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy

install_foreign test-pkgbase-provides-legacy
assert_pkg_installed test-pkgbase-provides-legacy

# Trigger: type the PKGBASE name. expand routes via by_name (the pkgbase
# resolves directly to its entry), Target.hint stays None. prepare_one
# calls counterpart_with_hint(entry, None) → counterpart_unhinted, which
# walks Pkgname (empty — new pkgnames not in localdb) → Replaces (empty,
# no replaces= declared) → Provides (pkgbase-level hits legacy).
RUST_LOG=gitaur=warn,gitaur=info gitaur -S --noconfirm test-pkgbase-provides-new
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

# Even without a hint, the unhinted walk must surface the legacy through
# pkgbase-level Provides (no scoped provides in this fixture). via must
# still be Provides — not None.
grep -qF 'via=Some(Provides)' <<<"$trace_line" || {
    echo "expected via=Some(Provides) — pkgbase-level provides must resolve without a hint" >&2
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

# No hint to diverge, single installed provider — both diagnostics
# silent.
if strip_ansi "$LAST_STDERR" | grep -q "multiple installed pkgs match this pkgbase's provides"; then
    echo "REGRESSION: multi-match warning fired with only one provided counterpart." >&2
    _dump >&2
    exit 1
fi

if strip_ansi "$LAST_STDERR" | grep -q 'counterpart hint diverged from unhinted lookup'; then
    echo "REGRESSION: divergence warning fired despite hint=None." >&2
    _dump >&2
    exit 1
fi
