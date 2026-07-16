# Shared helpers for aurox container tests.
#
# Each test script in smoke/ or extended/ sources this file, then declares
# its scenario as a sequence of `aurox …` invocations and `assert_*`
# checks. Think Playwright fixtures: setup is implicit, assertions are
# explicit, failure halts the script.
#
# Conventions:
#   * Tests run as the `builder` user inside the container.
#   * `bootstrap` (called once per test) wires:
#       - /etc/pacman.conf to include the local sync repo at /srv/local-repo
#       - aurox's config to point at file:///srv/mock-aur as the AUR mirror
#       - a clean ~/.local/state/aurox
#   * Network access is never required — fixtures cover everything.

set -euo pipefail

AUROX="${AUROX:-/work/target/debug/aurox}"
# PTY/HTTP driver examples live next to whichever aurox binary is in use, so
# they resolve for both the default target dir and the coverage build's
# (target/coverage-build/debug/examples) without a hardcoded path.
# shellcheck disable=SC2034  # consumed by the sourcing test scripts, not here
EXAMPLES_DIR="$(dirname "$AUROX")/examples"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/aurox"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/aurox"
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
# Point aurox at the on-disk fake mirror instead of github.com/archlinux/aur.
mirror_url = "file://$MOCK_AUR"
# The image's /etc/pacman.conf carries the real [core]/[extra]; leaving the
# rootless official-repo sync on would make every refresh smoke hit the network,
# breaking the suite's no-network guarantee. Off by default; the dedicated
# rootless-sync smoke flips it back on against a hermetic local-repo-only config.
check_repo_updates = false
EOF
}

# Wipe aurox's state between tests (NOT pacman state — the container is the
# rollback boundary; one test per container run via the harness).
reset_state() {
    rm -rf "$STATE_DIR"
    mkdir -p "$STATE_DIR"
}

# Run aurox, capturing stdout/stderr/exit into LAST_*.
aurox() {
    LAST_STDOUT="$(mktemp)"
    LAST_STDERR="$(mktemp)"
    set +e
    "$AUROX" "$@" >"$LAST_STDOUT" 2>"$LAST_STDERR"
    LAST_EXIT=$?
    set -e
}

# Like `aurox`, but feeds the first argument to stdin so the interactive
# confirm path is exercised (no `--noconfirm`). stdin is not a TTY here, so
# `ui::confirm` takes its line-read fallback: each prompt consumes one input
# line, and an empty string (EOF on the first read) is treated as the "yes"
# default. Use "y"/"n" lines to answer successive prompts deterministically.
aurox_input() {
    local input="$1"; shift
    LAST_STDOUT="$(mktemp)"
    LAST_STDERR="$(mktemp)"
    set +e
    printf '%s' "$input" | "$AUROX" "$@" >"$LAST_STDOUT" 2>"$LAST_STDERR"
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

assert_stdout_not_contains() {
    ! grep -qF -- "$1" "$LAST_STDOUT" || { echo "stdout unexpectedly contains: $1" >&2; _dump >&2; return 1; }
}

assert_stderr_contains() {
    grep -qF -- "$1" "$LAST_STDERR" || { echo "stderr missing: $1" >&2; _dump >&2; return 1; }
}

assert_stderr_not_contains() {
    ! grep -qF -- "$1" "$LAST_STDERR" || { echo "stderr unexpectedly contains: $1" >&2; _dump >&2; return 1; }
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

# ---------------------------------------------------------------------------
# Foreign-install seed
#
# Install a `repo=foreign` fixture's pre-built artifact via pacman -U. The
# resulting state — pkgname in localdb but not in any sync DB and not an AUR
# pkgbase — models the dotnet-runtime case the resolver's `by_provides` walk
# is designed to handle. Used as a seed in tests that exercise hint-driven
# counterpart resolution (32, 33).
install_foreign() {
    local pkgbase="$1"
    local pkg
    pkg=$(ls /srv/foreign-pkgs/"$pkgbase"-*.pkg.tar.zst 2>/dev/null | head -1)
    [[ -n "$pkg" ]] || { echo "no foreign artifact for $pkgbase under /srv/foreign-pkgs/" >&2; return 1; }
    sudo pacman -U --noconfirm "$pkg" >/dev/null
}
