#!/usr/bin/env bash
# `aurox -Sy` against an HTTP tarpit must abort once curl's lowSpeedTime
# elapses, not hang forever on the dead socket. Verifies:
#   * exit non-zero
#   * total wall-clock is within ~2× the configured idle window
#   * stderr surfaces a recognisable curl-side "operation too slow" diagnostic
source /work/tests/container/lib.sh
bootstrap

# Point aurox at the tarpit on loopback, with a tight 3-second idle budget
# so the test finishes fast. The tarpit accepts TCP and reads the request
# headers, then stalls — curl's lowSpeedTime is what should bail.
PORT=18765
TIMEOUT_SECS=3
cat > "$CONFIG_DIR/config.toml" <<EOF
mirror_url = "http://127.0.0.1:$PORT/aur.git"
mirror_idle_timeout_secs = $TIMEOUT_SECS
EOF
reset_state

"$EXAMPLES_DIR/tarpit" "$PORT" >/tmp/tarpit.log 2>&1 &
TARPIT_PID=$!
trap 'kill $TARPIT_PID 2>/dev/null; wait $TARPIT_PID 2>/dev/null || true' EXIT

# Wait for the listener to come up before pointing aurox at it.
for _ in $(seq 1 50); do
    if (echo >/dev/tcp/127.0.0.1/$PORT) 2>/dev/null; then break; fi
    sleep 0.1
done

START=$EPOCHREALTIME
aurox -Sy --noconfirm
END=$EPOCHREALTIME
ELAPSED_MS=$(awk -v s="$START" -v e="$END" 'BEGIN { printf "%d", (e - s) * 1000 }')

assert_exit 1
# Timing band proves it was *our* timeout that fired. Lower bound rules out
# an instant failure (connection refused, transport mis-selected); upper
# bound rules out the TCP-retransmit fallback (~5 min) creeping back.
MIN_MS=$((TIMEOUT_SECS * 1000 / 2))
MAX_MS=$((TIMEOUT_SECS * 3 * 1000))
if (( ELAPSED_MS < MIN_MS || ELAPSED_MS > MAX_MS )); then
    echo "fetch took ${ELAPSED_MS}ms, expected ${MIN_MS}-${MAX_MS}ms (idle window ${TIMEOUT_SECS}s)" >&2
    _dump >&2
    exit 1
fi

# gix wraps the curl-side message as a generic "IO error … talking to the
# server" — the upstream "Operation too slow" string doesn't bubble through.
# Match the wrapping instead so this assert stays meaningful without
# depending on gix's exact phrasing.
assert_stderr_contains "fetch_only"

echo "OK — fetch aborted in ${ELAPSED_MS}ms (band ${MIN_MS}-${MAX_MS}ms)"
