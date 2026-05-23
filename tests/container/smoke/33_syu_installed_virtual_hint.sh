#!/usr/bin/env bash
# `-Syu` flow for an installed foreign virtual whose AUR pkgbase declares
# multiple installed virtuals as `provides=`. This is the dotnet-runtime
# regression as the user actually experienced it (after Phase B's typed
# Version and hint plumbing landed):
#
#   * test-syu-hint-newer is installed at 9.0 (foreign, vercmp-newer than
#     the new pkgbase at 2.0) — so `-Syu` does NOT include it in the
#     picker.
#   * test-syu-hint-older is installed at 1.0 (foreign, outdated) — `-Syu`
#     shows it as an upgrade row → 2.0.
#   * test-syu-hint-new declares `provides=("test-syu-hint-newer"
#     "test-syu-hint-older")` (newer first).
#
# The user picks test-syu-hint-older (auto-selected via --noconfirm). The
# `expand_pkgbase_targets` pacman-shortcut would otherwise drop the hint
# (`pac.is_installed("test-syu-hint-older")` is true → passthrough),
# leaving `prepare_one` to call `counterpart_with_hint` with `None`. The
# unhinted walk then returns test-syu-hint-newer (first declared,
# installed) — visibly wrong, and what the original bug report showed.
#
# After the `record_target_hint` fix, the hint is recorded at the top of
# the expand loop regardless of the passthrough decision, so counterpart
# returns the user's actual intent.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy

# Seed: install BOTH legacy halves. Each is its own pkgbase with no
# provides, so `gitaur -S` registers them in localdb as foreign pkgs.
gitaur -S --noconfirm test-syu-hint-newer
assert_exit 0
assert_pkg_installed test-syu-hint-newer

gitaur -S --noconfirm test-syu-hint-older
assert_exit 0
assert_pkg_installed test-syu-hint-older

# Trigger: full `-Syu` cycle. The picker (with --noconfirm) auto-selects
# every upgrade row. test-syu-hint-newer (installed 9.0 > available 2.0)
# is NOT in the picker; test-syu-hint-older (installed 1.0 < available
# 2.0) IS. So the only AUR row picked maps to pkgbase test-syu-hint-new
# with hint=test-syu-hint-older.
RUST_LOG=gitaur=warn,gitaur=info gitaur -Su --noconfirm
assert_exit 0
assert_pkg_installed test-syu-hint-new

# `tracing_subscriber::fmt::layer()` always emits ANSI escapes around the
# `=` field separators regardless of TTY detection. Strip before
# grepping so the field patterns can be literal.
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

# The auto-proceeding trace for test-syu-hint-new must show the
# hint-driven counterpart (older), not the first-declared default (newer).
trace_line=$(strip_ansi "$LAST_STDERR" | grep 'auto-proceeding.*test-syu-hint-new' | head -1 || true)
[[ -n "$trace_line" ]] || {
    echo "missing auto-proceeding trace for test-syu-hint-new" >&2
    _dump >&2
    exit 1
}

grep -qF 'via=Some(Provides)' <<<"$trace_line" || {
    echo "expected via=Some(Provides) in trace, got: $trace_line" >&2
    _dump >&2
    exit 1
}

# The crucial assertion: counterpart is test-syu-hint-older (what the
# user actually saw in the picker and selected), NOT test-syu-hint-newer
# (the declaration-order winner under the unhinted walk).
grep -qF 'installed=Some(PkgName("test-syu-hint-older"))' <<<"$trace_line" || {
    echo "expected installed=Some(PkgName(\"test-syu-hint-older\")) — hint failed to reach counterpart" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

# Explicit guard against the regression: the WRONG name must NOT appear
# as the resolved counterpart. Without the fix, the trace shows
# test-syu-hint-newer (first declared, installed). Make that an outright
# failure rather than relying only on the positive assertion above.
if grep -qF 'installed=Some(PkgName("test-syu-hint-newer"))' <<<"$trace_line"; then
    echo "REGRESSION: counterpart resolved to test-syu-hint-newer (the first-declared" >&2
    echo "  installed provides). The hint plumbing didn't reach prepare_one. Trace:" >&2
    echo "  $trace_line" >&2
    _dump >&2
    exit 1
fi

# Hint-divergence warning: the unhinted walk would have picked newer
# (first declared, installed); the hint changed that to older. The
# diagnostic must fire so future debuggers see the rescue in the log.
strip_ansi "$LAST_STDERR" | grep -q 'counterpart hint diverged from unhinted lookup' || {
    echo "expected 'counterpart hint diverged from unhinted lookup' warning in stderr" >&2
    echo "  (the hint changed counterpart from test-syu-hint-newer to test-syu-hint-older;" >&2
    echo "   without this warning, the divergence is silent and future regressions are" >&2
    echo "   harder to spot)" >&2
    _dump >&2
    exit 1
}
