#!/usr/bin/env bash
# The hard-fail half of mirror unreachability (the dead-socket timeout half
# is extended/01): with the mock AUR moved away, `-Sy` must fail cleanly —
# and everything local must keep working. The existing index still serves
# `-Ss`, and `-S` still builds+installs from the local bare mirror: aurox's
# "fully offline-from-AUR" stance, end to end.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

sudo mv "$MOCK_AUR" "${MOCK_AUR}.gone"

aurox -Sy
[[ "$LAST_EXIT" != "0" ]] || { echo "expected -Sy to fail with the mirror gone"; _dump; exit 1; }

# Reads keep working from the on-disk index …
aurox -Ss '^test-trivial$'
assert_exit 0
assert_stdout_contains "aur/test-trivial"

# … and installs keep working from the local mirror clone.
aurox -S --noconfirm test-trivial
assert_exit 0
assert_pkg_installed test-trivial
