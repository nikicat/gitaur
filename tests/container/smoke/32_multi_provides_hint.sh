#!/usr/bin/env bash
# `-S` of an AUR pkgbase that declares MULTIPLE `provides=` virtuals, more
# than one of which the user has installed. Regression target: the
# aspnet-runtime / dotnet-runtime-7.0 case — without a counterpart hint,
# `PacmanIndex::counterpart` picks the first declared provides (which the
# user did not type and which is the wrong upgrade lineage), and the
# review screen labels the build as a fresh `install:` with no diff.
#
# After the hint plumbing, `expand_pkgbase_targets` records the user's
# typed pkgname as `Plan.counterpart_hints[<pkgbase>]`, and
# `counterpart_with_hint` returns the hinted match (Provides provenance
# on the SECOND-declared virtual). The noconfirm trace records all of:
#   * via = Provides
#   * installed = the legacy pkgname the user typed (legacy-b)
#   * `hint diverged from unhinted lookup` warning fired
#   * `multiple installed pkgs match this pkgbase's provides` warning fired
#
# Together these prove the hint flowed end-to-end AND that the ambiguity
# diagnostics surface the case for future debuggers.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy

# Seed: install BOTH legacy pkgs. Each is its own pkgbase with no provides
# declarations — the multi-match scenario requires both to be present in
# localdb so the new pkgbase's `provides=` array has two installed
# candidates to choose from.
gitaur -S --noconfirm test-multi-provides-legacy-a
assert_exit 0
assert_pkg_installed test-multi-provides-legacy-a

gitaur -S --noconfirm test-multi-provides-legacy-b
assert_exit 0
assert_pkg_installed test-multi-provides-legacy-b

# Trigger: install the new pkgbase by typing the SECOND-declared virtual.
# The PKGBUILD lists legacy-a first, then legacy-b, so the unhinted walk
# would pick legacy-a. The hint plumbing must override that and pin the
# counterpart to legacy-b.
RUST_LOG=gitaur=warn,gitaur=info gitaur -S --noconfirm test-multi-provides-legacy-b
assert_exit 0
assert_pkg_installed test-multi-provides-new

# `tracing_subscriber::fmt::layer()` always emits ANSI escapes around the
# `=` field separators regardless of TTY detection. Strip them before
# grepping so the field patterns can be literal.
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

# The auto-proceeding trace for test-multi-provides-new must show the
# hint-driven counterpart, not the first-declared default.
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

# The crucial assertion: counterpart is legacy-b (what the user typed),
# NOT legacy-a (declaration-order winner under the unhinted walk).
grep -qF 'installed=Some(PkgName("test-multi-provides-legacy-b"))' <<<"$trace_line" || {
    echo "expected installed=Some(PkgName(\"test-multi-provides-legacy-b\")) — hint failed to steer counterpart" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

# Ambiguity diagnostics — neither is mandatory for correctness but they
# document the hint's effect for future debuggers. The "diverged" warning
# fires because the unhinted walk would've picked legacy-a and the hint
# changed that to legacy-b.
strip_ansi "$LAST_STDERR" | grep -q 'counterpart hint diverged from unhinted lookup' || {
    echo "expected 'counterpart hint diverged from unhinted lookup' warning in stderr" >&2
    _dump >&2
    exit 1
}

# The multi-match warning fires from the unhinted walk (which still runs
# inside counterpart_with_hint so we can detect divergence). Both legacy
# pkgs are installed candidates, so the warning must mention legacy-a as
# picked and legacy-b as an alternative.
strip_ansi "$LAST_STDERR" | grep -q "multiple installed pkgs match this pkgbase's provides" || {
    echo "expected 'multiple installed pkgs match' warning in stderr" >&2
    _dump >&2
    exit 1
}
