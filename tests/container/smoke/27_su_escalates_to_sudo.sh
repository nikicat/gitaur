#!/usr/bin/env bash
# Regression: `gitaur -Su` previously slipped past the sudo allowlist (only
# exact strings -S/-Sy/-Syu/-Syyu/-Sc/-Scc were recognised) and reached
# pacman unescalated, producing "you cannot perform this operation unless
# you are root". The classifier now recognises arbitrary -S clusters, so
# the upgrade actually runs.
#
# Without root the container's fixture user can't even start pacman -Su;
# success here proves the sudo gate fired. We don't need pacman to find
# anything to upgrade — exit 0 with no "must be root" complaint is enough.
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Su --noconfirm
assert_exit 0
# The bug surfaced as this exact pacman message on stderr.
grep -qi "must be root\|cannot perform this operation unless you are root" \
    "$LAST_STDERR" && {
    echo "regression: pacman ran unescalated under -Su" >&2
    cat "$LAST_STDERR" >&2
    exit 1
} || true
