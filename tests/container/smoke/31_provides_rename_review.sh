#!/usr/bin/env bash
# `-S` of a new AUR pkgbase that `provides=` an already-installed legacy pkg
# of a different name. Regression target: the dotnet-runtime-7.0 case — user
# has the legacy name in their localdb (from an older AUR pkgbase no longer
# in the index), the new pkgbase declares `provides=<legacy>`, and the review
# screen used to label this as a fresh `install:` with full PKGBUILD because
# `prepare_one` only checked `entry.pkgnames[*].name` against the localdb.
#
# After the counterpart helper, `PacmanIndex::counterpart` walks
# pkgname → replaces → provides and tags the match with provenance. Under
# `--noconfirm` the review's `info!` event carries that provenance — this
# script asserts the trace shape end-to-end through alpm.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy

# Seed: install the legacy pkg under its own name. From pacman's view this is
# now a "foreign" install (the legacy pkgbase isn't in any sync DB), which is
# exactly the dotnet-shaped starting state.
gitaur -S --noconfirm test-provides-rename-legacy
assert_exit 0
assert_pkg_installed test-provides-rename-legacy

# Trigger: install the new pkgbase. Its pkgname is unrelated to anything in
# the localdb, but it declares `provides=test-provides-rename-legacy`. The
# counterpart helper must find the legacy install via Provides; the noconfirm
# trace records that.
RUST_LOG=gitaur=info gitaur -S --noconfirm test-provides-rename-new
assert_exit 0
assert_pkg_installed test-provides-rename-new

# `tracing_subscriber::fmt::layer()` always emits ANSI escapes around field
# separators (the dimming on `=`), regardless of TTY detection. Strip them
# before grepping so the trace shape can be matched with literal patterns.
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

# The trace line for this pkgbase must reflect:
#   * via = Provides (not Pkgname, not None — Pkgname would mean we matched
#     test-provides-rename-new in the localdb, which it isn't; None would
#     mean we missed the legacy install entirely, which is the bug).
#   * installed = Some(PkgName("test-provides-rename-legacy")) — the legacy
#     pkgname is what the counterpart helper resolved. (PkgName is a typed
#     newtype with its own Debug impl, which the trace emits verbatim.)
trace_line=$(strip_ansi "$LAST_STDERR" | grep 'auto-proceeding.*test-provides-rename-new' | head -1 || true)
[[ -n "$trace_line" ]] || {
    echo "missing auto-proceeding trace for test-provides-rename-new" >&2
    _dump >&2
    exit 1
}

# Each assertion narrates which fragment of the trace it's verifying so a
# failure points at the specific provenance bit that went wrong.
grep -qF 'via=Some(Provides)' <<<"$trace_line" || {
    echo "expected via=Some(Provides) in trace, got: $trace_line" >&2
    _dump >&2
    exit 1
}
grep -qF 'installed=Some(PkgName("test-provides-rename-legacy"))' <<<"$trace_line" || {
    echo "expected installed=Some(PkgName(\"test-provides-rename-legacy\")) in trace, got: $trace_line" >&2
    _dump >&2
    exit 1
}

# (No Pkgname sanity check: a same-version reinstall of the legacy pkg
# would short-circuit on `prepare_one`'s build-artifact cache and never
# reach review() — that path is exercised by every other smoke test that
# installs a canonical fixture, and unit-tested in alpm_db::tests.)
