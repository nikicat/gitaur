#!/usr/bin/env bash
# The README hero-demo driver, run as a plain test so the demo can't rot: the
# paced search → stage → review gate → apply flow must keep working exactly as
# recorded, or this fails before a stale GIF ever ships. See
# docs/plans/screencasts.md; the driver doc explains the pacing helpers.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Build the on-disk index so the seeded search classifies test-hello as AUR.
aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/demo_search_install"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "demo driver failed (search / stage / review gate / apply)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'DEMO_SEARCH_INSTALL_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The demo's apply really installs — the on-screen `done` must match reality.
assert_pkg_installed test-hello
