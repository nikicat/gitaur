#!/usr/bin/env bash
# Two concurrent `aurox -Sy`s must coexist: the RefreshLock (an advisory
# flock on aurox's private dbpath) serializes the rootless repo-DB sync, so
# neither run may trip libalpm's bare-existence `db.lck` ("unable to lock
# database") and both must exit 0. The AUR half races too — concurrent gix
# fetches + index writes — and the state must come out servable.
#
# check_repo_updates is on (suite default off) with the hermetic local-only
# pacman.conf from smoke/55, so the lock's critical section actually runs
# and no network is touched.
source /work/tests/container/lib.sh
bootstrap; reset_state

cat > "$CONFIG_DIR/config.toml" <<EOF
mirror_url = "file://$MOCK_AUR"
check_repo_updates = true
EOF
awk '/^\[/ { keep = ($0 == "[options]" || $0 == "[local-repo]") } keep' \
    /etc/pacman.conf > /tmp/pacman.conf.hermetic
sudo cp /tmp/pacman.conf.hermetic /etc/pacman.conf

# Prime the mirror clone once so the racers contend on refresh, not bootstrap.
aurox -Sy
assert_exit 0

out_a=$(mktemp); out_b=$(mktemp)
set +e
"$AUROX" -Sy >"$out_a" 2>&1 &
pid_a=$!
"$AUROX" -Sy >"$out_b" 2>&1 &
pid_b=$!
wait "$pid_a"; exit_a=$?
wait "$pid_b"; exit_b=$?
set -e

for f in "$out_a" "$out_b"; do
    if grep -q 'unable to lock database' "$f"; then
        echo "a concurrent -Sy tripped libalpm's db.lck:" >&2
        cat "$f" >&2
        exit 1
    fi
done
[[ "$exit_a" == "0" && "$exit_b" == "0" ]] || {
    echo "concurrent -Sy runs must both succeed (got $exit_a / $exit_b)" >&2
    echo "---- run A ----" >&2; cat "$out_a" >&2
    echo "---- run B ----" >&2; cat "$out_b" >&2
    exit 1
}

# The shared state came out intact: the index serves and the private sync DB
# is populated.
aurox -Ss '^test-trivial$'
assert_exit 0
assert_stdout_contains "aur/test-trivial"
[[ -f "$STATE_DIR/syncdb/sync/local-repo.db" ]] || {
    echo "private sync db missing after concurrent refreshes" >&2
    _dump >&2
    exit 1
}
