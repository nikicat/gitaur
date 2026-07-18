#!/usr/bin/env bash
# The Ctrl-C-during-repo-refresh demo driver, run as a plain test so the
# recorded flow can't rot. Ctrl-C during the official-repo DB refresh must
# abort the download *promptly* and bail to a live prompt — libalpm's
# internal downloader can't be interrupted from outside (pacman itself just
# _Exits on ^C), so aurox registers its own fetch callback (pacman/dload.rs)
# whose curl progress meter watches the SIGINT flag; this pins that whole
# path end-to-end.
#
# The seed (demos/seed-ctrlc-repo-refresh.sh — one seed, two consumers with
# demos/build.sh) makes pacman.conf hermetic and points [local-repo] at
# hung_mirror, a server that answers headers then goes silent, so
# `refresh pacman` parks mid-download. The driver sends the real ^C byte,
# asserts the "official-repo refresh interrupted" note within seconds, and
# leaves via the idle-prompt ^C (extended/38's contract), asserting exit 130.
source /work/tests/container/lib.sh
bootstrap; reset_state

source /work/demos/seed-ctrlc-repo-refresh.sh

# Bootstrap the mirror + index (repo sync still off — the driver flips it on)
# so the shell opens at its normal prompt instead of the first-launch consent
# question.
aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/demo_ctrlc_repo_refresh"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "ctrl-c repo-refresh demo driver failed (refresh / interrupt / prompt / exit)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'DEMO_CTRLC_REPO_REFRESH_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

echo "OK — repo-db refresh interrupted promptly, shell survived to the ^C quit"
