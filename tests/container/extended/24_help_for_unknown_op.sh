#!/usr/bin/env bash
# The pre-scan forwards *known* pacman ops (-R/-Q/-T/…) verbatim; an op
# nobody owns lands in aurox's own dispatch, which must fail loudly with
# its "unsupported aurox op" diagnostic pointing at `aurox --help`, and a
# nonzero exit — not a panic, not a silent 0, and not a clap usage error
# swallowing the op letter.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Z
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit for unknown op"; _dump; exit 1; }
assert_stderr_contains "unsupported aurox op"
assert_stderr_contains "-Z"
assert_stderr_contains "aurox --help"

# The real help paths stay intact next to it (smoke/15 pins -h == --help).
aurox --help
assert_exit 0
assert_stdout_contains "aurox"
