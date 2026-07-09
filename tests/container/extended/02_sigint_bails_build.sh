#!/usr/bin/env bash
# A SIGINT during an AUR build must be *caught* (aurox survives), *forwarded*
# to makepkg's process group (the build child dies, not orphaned), and *reported*
# as an interrupt (exit 1 + "build interrupted") — never kill aurox outright.
#
# This drives the `-S` path: the no-arg loop's bail-to-table re-entry needs a
# PTY/expect harness, but the signal mechanism in `build::makepkg::run` is the
# same code, and that's what this exercises. We deliver SIGINT to aurox's pid
# the way a keyboard Ctrl+C would deliver it to the foreground process.
source /work/tests/container/lib.sh
bootstrap
reset_state

aurox -Sy --noconfirm

out="$(mktemp)"
err="$(mktemp)"
"$AUROX" -S --noconfirm test-sleep-build >"$out" 2>"$err" &
AUROX_PID=$!
# Clean up if any assertion bails: kill aurox and any orphaned sleep.
trap 'kill -INT "$AUROX_PID" 2>/dev/null || true; pkill -f "sleep 3137" 2>/dev/null || true; wait "$AUROX_PID" 2>/dev/null || true' EXIT

# Wait for build() to start — its sentinel reaches the tee'd stdout / build.log.
log="$STATE_DIR/pkgs/test-sleep-build/build.log"
started=0
for _ in $(seq 1 200); do
    if grep -qsF 'AUROX_SLEEP_BUILD_SENTINEL' "$out" "$log" 2>/dev/null; then
        started=1
        break
    fi
    # If aurox already exited, the build never reached the sleep — fail fast.
    kill -0 "$AUROX_PID" 2>/dev/null || break
    sleep 0.1
done
if [[ "$started" != 1 ]]; then
    echo "build never started" >&2
    echo "---- stdout ----" >&2; cat "$out" >&2
    echo "---- stderr ----" >&2; cat "$err" >&2
    exit 1
fi

# Interrupt the build the way Ctrl+C would: SIGINT to aurox, not the child.
kill -INT "$AUROX_PID"

# `wait` blocks until aurox exits and yields its status (a background job that
# already exited still reports via `wait`, unlike `kill -0`, which keeps
# succeeding on the un-reaped zombie). A watchdog SIGKILLs aurox if it hangs on
# the dead build, so the test fails loudly instead of stalling.
(
    sleep 15
    kill -KILL "$AUROX_PID" 2>/dev/null || true
) &
WATCHDOG=$!
set +e
wait "$AUROX_PID"
exit_code=$?
set -e
kill "$WATCHDOG" 2>/dev/null || true
wait "$WATCHDOG" 2>/dev/null || true

# Exit 1 is our interrupted path; 130 (128+SIGINT) would mean the default
# action killed aurox (handler never ran), and 137 (128+SIGKILL) means the
# watchdog fired because aurox hung.
if [[ "$exit_code" != 1 ]]; then
    echo "expected exit 1 (caught interrupt), got $exit_code" >&2
    echo "---- stderr ----" >&2; cat "$err" >&2
    exit 1
fi
grep -qF 'build interrupted' "$err" || {
    echo "stderr missing 'build interrupted' — interrupt not reported" >&2
    cat "$err" >&2
    exit 1
}

# Forwarding worked iff makepkg's sleeping child was reaped, not orphaned.
sleep 0.5
if pgrep -f 'sleep 3137' >/dev/null 2>&1; then
    echo "makepkg child survived — SIGINT was not forwarded to the process group" >&2
    pgrep -af 'sleep 3137' >&2
    exit 1
fi

# The build never finished, so nothing should have been installed.
assert_pkg_not_installed test-sleep-build

echo "OK — SIGINT caught, forwarded to makepkg group, reported as interrupt (exit $exit_code)"
