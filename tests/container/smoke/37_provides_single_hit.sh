#!/usr/bin/env bash
# Case 7 of the counterpart-resolution matrix
# (`docs/ARCHITECTURE.md#resolution-case-matrix`): the new pkgbase declares
# MULTIPLE `provides=` virtuals but only ONE is installed in the user's
# localdb. The counterpart helper must land on Provides cleanly with no
# multi-match diagnostic — the warning exists for genuine ambiguity (case
# 8), not for the common single-hit shape.
#
# Reuses test-multi-provides-new (declares legacy-a, legacy-b) and the
# legacy-a foreign artifact already baked into the image, so the only
# new code is this script.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy

# Seed: only legacy-a is foreign-installed. legacy-b is intentionally
# absent — that's the distinguishing condition versus smoke 32.
install_foreign test-multi-provides-legacy-a
assert_pkg_installed test-multi-provides-legacy-a
assert_pkg_not_installed test-multi-provides-legacy-b

# Trigger: install the new pkgbase by typing the only-installed legacy.
# expand_pkgbase_targets rewrites the spec to the pkgbase and derives a
# hint of legacy-a. counterpart_with_hint and the unhinted walk both
# converge on legacy-a (no ambiguity), so no diagnostics should fire.
RUST_LOG=gitaur=warn,gitaur=info gitaur -S --noconfirm test-multi-provides-legacy-a
assert_exit 0
assert_pkg_installed test-multi-provides-new

strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

trace_line=$(strip_ansi "$LAST_STDERR" | grep 'auto-proceeding.*test-multi-provides-new' | head -1 || true)
[[ -n "$trace_line" ]] || {
    echo "missing auto-proceeding trace for test-multi-provides-new" >&2
    _dump >&2
    exit 1
}

grep -qF 'via=Some(Provides)' <<<"$trace_line" || {
    echo "expected via=Some(Provides) in trace, got: $trace_line" >&2
    _dump >&2
    exit 1
}

grep -qF 'installed=Some(PkgName("test-multi-provides-legacy-a"))' <<<"$trace_line" || {
    echo "expected installed=Some(PkgName(\"test-multi-provides-legacy-a\")) in trace" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

# Diagnostics must NOT fire on the single-hit path. The multi-match
# warning is for the dotnet-runtime shape (case 8); the divergence
# warning is for hint-rescued mismatches. Both quiet here means the
# Provides tier collapsed cleanly to one installed candidate AND the
# hint matched the unhinted walk's pick.
if strip_ansi "$LAST_STDERR" | grep -q "multiple installed pkgs match this pkgbase's provides"; then
    echo "REGRESSION: multi-match warning fired with only one installed provider." >&2
    _dump >&2
    exit 1
fi

if strip_ansi "$LAST_STDERR" | grep -q 'counterpart hint diverged from unhinted lookup'; then
    echo "REGRESSION: hint-divergence warning fired on the agreed single-hit path." >&2
    _dump >&2
    exit 1
fi
