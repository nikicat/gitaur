#!/usr/bin/env bash
# --noresync opts out of the auto-rebuild: a stale index becomes a clean error
# pointing at `-Sy`, and the on-disk index is left untouched (no implicit
# network fetch + rebuild). Proves the flag is wired all the way through
# argv → runopts TLS → load_or_resync.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy
assert_exit 0
echo "this is not a valid rkyv archive" > "$STATE_DIR/index.bin"
before="$(cat "$STATE_DIR/index.bin")"

gaur --noresync test-trivial
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit under --noresync" >&2; _dump >&2; exit 1; }
assert_stderr_contains "noresync"

# The planted garbage must survive byte-for-byte — confirms no rebuild slipped
# through despite the flag.
after="$(cat "$STATE_DIR/index.bin")"
[[ "$before" == "$after" ]] || { echo "index.bin changed under --noresync" >&2; _dump >&2; exit 1; }
