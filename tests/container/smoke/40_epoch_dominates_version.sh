#!/usr/bin/env bash
# Epoch dominates pkgver comparison — libalpm vercmp says `2:0.1-1` is
# newer than `0.1-1` regardless of how the bare versions compare.
# Gitaur's upgrade pipeline must honour that, both at the
# counterpart-resolution step (Pkgname-tier match registers as upgrade,
# not reinstall) and at the pacman -U handoff (no `--needed` short-
# circuit, no downgrade refusal).
#
# Listed in `extended/.scope` since the test-epoch fixture has lived in
# the repo with no smoke coverage. Promoted to smoke because the path
# is part of the standard `-S` upgrade flow.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy

# Seed: foreign install at 0.1-1 (no epoch). Same pkgname as the AUR
# fixture so the new pkgbase will be a canonical Pkgname-tier match.
install_foreign test-epoch
assert_pkg_installed test-epoch

pacman -Q test-epoch | grep -qF '0.1-1' || {
    echo "expected installed test-epoch to be at 0.1-1 (no epoch)" >&2
    pacman -Q test-epoch >&2
    exit 1
}

# Trigger: install the AUR pkgbase at epoch=2, pkgver=0.1.
RUST_LOG=gitaur=info gitaur -S --noconfirm test-epoch
assert_exit 0
assert_pkg_installed test-epoch

# After the upgrade the localdb must show the epoch-prefixed version.
# Without epoch dominance pacman would have refused the upgrade as a
# downgrade-or-equal.
pacman -Q test-epoch | grep -qF '2:0.1-1' || {
    echo "expected test-epoch to be at 2:0.1-1 after upgrade, got:" >&2
    pacman -Q test-epoch >&2
    exit 1
}

strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

trace_line=$(strip_ansi "$LAST_STDERR" | grep 'auto-proceeding.*test-epoch' | head -1 || true)
[[ -n "$trace_line" ]] || {
    echo "missing auto-proceeding trace for test-epoch" >&2
    _dump >&2
    exit 1
}

# Pkgname tier — same pkgname under different versions.
grep -qF 'via=Some(Pkgname)' <<<"$trace_line" || {
    echo "expected via=Some(Pkgname) — same pkgname, epoch-disambiguated upgrade" >&2
    echo "  got: $trace_line" >&2
    _dump >&2
    exit 1
}

# Trace must distinguish the two versions: installed_version is the
# no-epoch one, new_ver is the epoch-prefixed one. If these collide,
# review would label the build as a `reinstall:` (case 2) and skip the
# diff — masking real upgrade work.
grep -qF 'new_ver=2:0.1-1' <<<"$trace_line" || {
    echo "expected new_ver=2:0.1-1 in trace, got: $trace_line" >&2
    _dump >&2
    exit 1
}

grep -qE 'installed_version=Some\(Ver\("0\.1-1"\)\)' <<<"$trace_line" || {
    echo "expected installed_version=Some(Ver(\"0.1-1\")) in trace, got: $trace_line" >&2
    _dump >&2
    exit 1
}
