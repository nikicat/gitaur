#!/usr/bin/env bash
# Bare `aurox <term>` lists AUR matches freshest-commit-first (not
# alphabetically). The three test-csort-* fixtures share the unique
# 'csortprobe' token but carry pinned, deliberately non-alphabetical commit
# dates (a=2020, b=2023, c=2021). Alphabetical order would be a,b,c; freshest
# first must be b,c,a — so the assertion fails loudly if the sort key ever
# reverts to pkgbase.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

aurox csortprobe
assert_exit 0

# Extract the pkgbase suffix of each csort row in listing order. The bare-term
# listing prints one `aur   <pkgbase> …` table row per hit.
order="$(grep -oE 'test-csort-[abc]' "$LAST_STDOUT" | sed 's/test-csort-//' | tr -d '\n')"
if [[ "$order" != "bca" ]]; then
    echo "expected freshest-first order 'bca' (b=2023,c=2021,a=2020), got '$order'" >&2
    _dump >&2
    exit 1
fi
