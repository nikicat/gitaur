#!/usr/bin/env bash
# Case 11 of the counterpart-resolution matrix
# (`docs/ARCHITECTURE.md#resolution-case-matrix`): pkgbase declares
# `replaces=` of its own pkgname (stale, left over from a real prior
# rename), and the user has the canonical pkgname installed at an older
# version. The counterpart helper MUST resolve through Pkgname (tier 1),
# not Replaces (tier 2). Without that priority, `find_installed_commit`
# walks the wrong lineage and the header gets a misleading `[replaces …]`
# annotation.
#
# Unit-tested in `pacman::alpm_db::tests` already; this fixture catches
# regressions in any future expand-side optimisation that would
# short-circuit before `prepare_one` runs the priority walk.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy

# Seed: foreign install of test-stale-replaces at v1.0. Same pkgname the
# AUR fixture publishes at v2.0, but staged as a foreign artifact so the
# starting state isn't "AUR canonical install" (which would trigger an
# upgrade reinstall via state.db, not the path under test).
install_foreign test-stale-replaces
assert_pkg_installed test-stale-replaces

# Trigger: build the AUR pkgbase at v2.0. Its PKGBUILD declares
# `replaces=('test-stale-replaces')` — the load-bearing stale rename.
RUST_LOG=gitaur=warn,gitaur=info gitaur -S --noconfirm test-stale-replaces
assert_exit 0
assert_pkg_installed test-stale-replaces

strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

trace_line=$(strip_ansi "$LAST_STDERR" | grep 'auto-proceeding.*test-stale-replaces' | head -1 || true)
[[ -n "$trace_line" ]] || {
    echo "missing auto-proceeding trace for test-stale-replaces" >&2
    _dump >&2
    exit 1
}

# Pkgname (tier 1) MUST win over Replaces (tier 2). The whole point of
# this fixture is the guard.
grep -qF 'via=Some(Pkgname)' <<<"$trace_line" || {
    echo "expected via=Some(Pkgname) in trace — Pkgname must beat stale Replaces" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

grep -qF 'installed=Some(PkgName("test-stale-replaces"))' <<<"$trace_line" || {
    echo "expected installed=Some(PkgName(\"test-stale-replaces\")) in trace" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

# Explicit regression guard: Replaces must NOT have won. Without the
# tier order this is what a future bug would produce.
if grep -qF 'via=Some(Replaces)' <<<"$trace_line"; then
    echo "REGRESSION: stale replaces= took precedence over the literal Pkgname match." >&2
    echo "  trace: $trace_line" >&2
    _dump >&2
    exit 1
fi
