# Shared helpers for gitaur container tests.
#
# Each test script in smoke/ or extended/ sources this file, then declares
# its scenario as a sequence of `gitaur …` invocations and `assert_*`
# checks. Think Playwright fixtures: setup is implicit, assertions are
# explicit, failure halts the script.
#
# Conventions:
#   * Tests run as the `builder` user inside the container.
#   * `bootstrap` (called once per test) wires:
#       - /etc/pacman.conf to include the local sync repo at /srv/local-repo
#       - gitaur's config to point at file:///srv/mock-aur as the AUR mirror
#       - a clean ~/.local/state/gitaur
#   * Network access is never required — fixtures cover everything.

set -euo pipefail

GITAUR="${GITAUR:-/work/target/debug/gitaur}"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/gitaur"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/gitaur"
LOCAL_REPO="${LOCAL_REPO:-/srv/local-repo}"
MOCK_AUR="${MOCK_AUR:-/srv/mock-aur}"
LAST_STDOUT=""
LAST_STDERR=""
LAST_EXIT=0

# ---------------------------------------------------------------------------
# Lifecycle

# One-time per test container: write a config.toml pointing at the baked-in
# mock AUR mirror. Fixtures, /srv/local-repo, and pacman.conf are already
# prepared in the image — see Dockerfile.
bootstrap() {
    mkdir -p "$STATE_DIR" "$CONFIG_DIR"
    cat > "$CONFIG_DIR/config.toml" <<EOF
# Point gitaur at the on-disk fake mirror instead of github.com/archlinux/aur.
mirror_url = "file://$MOCK_AUR"
EOF
}

# Wipe gitaur's state between tests (NOT pacman state — the container is the
# rollback boundary; one test per container run via the harness).
reset_state() {
    rm -rf "$STATE_DIR"
    mkdir -p "$STATE_DIR"
}

# Run gitaur, capturing stdout/stderr/exit into LAST_*.
gitaur() {
    LAST_STDOUT="$(mktemp)"
    LAST_STDERR="$(mktemp)"
    set +e
    "$GITAUR" "$@" >"$LAST_STDOUT" 2>"$LAST_STDERR"
    LAST_EXIT=$?
    set -e
}

# ---------------------------------------------------------------------------
# Assertions — each one dumps captured output before failing for diagnostics.

_dump() {
    echo "---- stdout ----"
    cat "$LAST_STDOUT" || true
    echo "---- stderr ----"
    cat "$LAST_STDERR" || true
    echo "---- exit ${LAST_EXIT} ----"
}

assert_exit() {
    [[ "$LAST_EXIT" == "$1" ]] || { echo "expected exit $1, got $LAST_EXIT" >&2; _dump >&2; return 1; }
}

assert_stdout_contains() {
    grep -qF -- "$1" "$LAST_STDOUT" || { echo "stdout missing: $1" >&2; _dump >&2; return 1; }
}

assert_stderr_contains() {
    grep -qF -- "$1" "$LAST_STDERR" || { echo "stderr missing: $1" >&2; _dump >&2; return 1; }
}

assert_pkg_installed() {
    pacman -Qi "$1" >/dev/null 2>&1 || { echo "expected $1 installed" >&2; return 1; }
}

assert_pkg_not_installed() {
    ! pacman -Qi "$1" >/dev/null 2>&1 || { echo "expected $1 NOT installed" >&2; return 1; }
}

# pacman records Install Reason as "Explicitly installed" or "Installed as a
# dependency for another package". This is the asdeps-bug regression test.
assert_pkg_explicit() {
    pacman -Qi "$1" 2>/dev/null | grep -q 'Install Reason.*Explicitly' \
        || { echo "expected $1 explicit, got: $(pacman -Qi "$1" | grep 'Install Reason')" >&2; return 1; }
}

assert_pkg_asdep() {
    pacman -Qi "$1" 2>/dev/null | grep -q 'Install Reason.*as a dependency' \
        || { echo "expected $1 as-dep, got: $(pacman -Qi "$1" | grep 'Install Reason')" >&2; return 1; }
}
