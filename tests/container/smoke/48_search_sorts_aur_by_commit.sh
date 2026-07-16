#!/usr/bin/env bash
# Bare `aurox <term>` ranks AUR matches by commit freshness (not
# alphabetically) and prints best-last, so the freshest hit lands nearest the
# prompt. The three test-csort-* fixtures share the unique 'csortprobe' token
# but carry pinned, deliberately non-alphabetical commit dates (a=2020,
# b=2023, c=2021). Alphabetical order would be a,b,c; freshest-ranked printed
# bottom-up must be a,c,b — so the assertion fails loudly if the sort key
# ever reverts to pkgbase or the print order flips back to best-first.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

aurox csortprobe
assert_exit 0

# Extract the pkgbase suffix of each csort row in listing order. The bare-term
# listing prints one `aur   <pkgbase> …` table row per hit.
order="$(grep -oE 'test-csort-[abc]' "$LAST_STDOUT" | sed 's/test-csort-//' | tr -d '\n')"
if [[ "$order" != "acb" ]]; then
    echo "expected freshest-last order 'acb' (a=2020,c=2021,b=2023), got '$order'" >&2
    _dump >&2
    exit 1
fi
