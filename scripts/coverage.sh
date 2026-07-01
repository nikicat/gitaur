#!/usr/bin/env bash
# Three-tier coverage orchestrator: rust tests, podman container tests, and
# combined. All cargo + llvm-cov work runs inside the test image so the host
# (or CI runner) only needs podman.
#
# Outputs (all under coverage/ at the repo root):
#   lcov-rust.info / lcov-podman.info / lcov-combined.info
#   summary-rust.txt / summary-podman.txt / summary-combined.txt
#
# Implementation:
#   * `cargo llvm-cov show-env --export-prefix` is sourced inside helper
#     containers to set RUSTFLAGS, CARGO_LLVM_COV_TARGET_DIR, LLVM_PROFILE_FILE.
#   * Profraw output is segregated into <target>/profraw/rust/ and
#     <target>/profraw/podman/ via per-run LLVM_PROFILE_FILE overrides; each
#     report then copies just the group(s) it wants flat into <target> (where
#     `report` actually looks — see step 3).
#   * Test containers run as `builder` (makepkg refuses root) and write to
#     profraw/podman/ through a :U-mounted bind. Build and report containers
#     run as root so files come back owned by the host user under rootless
#     podman (container UID 0 maps to the running host user).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

CONTAINER="${CONTAINER:-podman}"
IMAGE="gitaur-test:latest"
# Keep this in sync with justfile's `ignore_regex` and codecov.yml.
IGNORE_REGEX='(examples/|src/main\.rs|src/testing\.rs)'

# Ensure the test image exists.
if ! "$CONTAINER" image exists "$IMAGE" 2>/dev/null; then
    "$CONTAINER" build -t "$IMAGE" -f tests/container/Dockerfile tests/container
fi

# Dedicated, container-only build directory. Deliberately NOT the default
# `target/llvm-cov-target` that the host `just coverage` recipes use — sharing
# it mixes host-built artifacts (referencing /home/$USER/.rustup paths) with
# container-built ones (referencing /work + /usr), which makes llvm-cov treat
# the same source file as two distinct entries and merge stale profraw.
COV_TARGET="$REPO_ROOT/target/coverage-build"

# Helper: run a shell snippet inside the test image with /work bind-mounted
# rw and CARGO_{HOME,TARGET_DIR} routed under target/ so artifacts persist on
# the host. The CARGO_TARGET_DIR override is essential — the Dockerfile pins
# it to /tmp/target for test containers (ephemeral), which would silently
# discard our instrumented build and profraw.
#
# LLVM_COV / LLVM_PROFDATA point cargo-llvm-cov at the system llvm tools: the
# image installs distro `rust` (not rustup), so the `llvm-tools-preview`
# component cargo-llvm-cov normally looks for is absent. Arch keeps `rust` and
# `llvm` on the same major LLVM release, so the formats are compatible.
in_image() {
    "$CONTAINER" run --rm --user 0:0 \
        -v "$REPO_ROOT:/work:rw" \
        -e CARGO_HOME=/work/target/coverage-cargo \
        -e CARGO_TARGET_DIR=/work/target/coverage-build \
        -e LLVM_COV=/usr/sbin/llvm-cov \
        -e LLVM_PROFDATA=/usr/sbin/llvm-profdata \
        -w /work \
        "$IMAGE" \
        bash -eu -c "$1"
}

# Podman profraw is collected into this staging subdir (rust profraw goes to a
# sibling rust/ subdir in step 1). Step 3 copies the wanted group(s) flat into
# COV_TARGET for reporting, since `cargo llvm-cov report` only discovers profraw
# sitting directly in CARGO_LLVM_COV_TARGET_DIR — it does not recurse.
PROFRAW_PODMAN="$COV_TARGET/profraw/podman"

overall_status=0

# Step 1 — wipe stale profraw/profdata (don't trust `cargo llvm-cov clean`,
# which leaves loose *.profraw behind), then build instrumented test + bin
# artifacts and run cargo tests with profraw routed into profraw/rust/. Build
# artifacts are kept for incremental rebuilds — only this container ever writes
# to COV_TARGET, so they stay path-consistent.
set +e
in_image '
    eval "$(cargo llvm-cov show-env --export-prefix)"
    DIR="$CARGO_LLVM_COV_TARGET_DIR"
    rm -rf "$DIR/profraw"
    find "$DIR" -maxdepth 1 \( -name "*.profraw" -o -name "*.profdata" \) -delete 2>/dev/null || true
    mkdir -p "$DIR/profraw/rust" "$DIR/profraw/podman"
    LLVM_PROFILE_FILE="$DIR/profraw/rust/%p-%m.profraw" \
        cargo test --all-features --locked --no-fail-fast
    # Build gaur plus the PTY/HTTP driver examples the extended tier shells out
    # to (shell_cart_e2e, shell_upgrade_e2e, tarpit, …). They land in the same
    # coverage-build dir, so tests/container/lib.sh finds them next to $GITAUR;
    # examples/ is coverage-ignored (IGNORE_REGEX) so they do not skew the report.
    cargo build --bin gaur --examples --locked
'
[[ $? -eq 0 ]] || { echo "scripts/coverage.sh: rust tests or instrumented build failed" >&2; overall_status=1; }
set -e

# Step 2 — run the full podman test suite (smoke + extended) using the
# instrumented binary built in step 1. The --coverage flag tells
# tests/container/run.sh to bind-mount $PROFRAW_PODMAN into each test container
# and set LLVM_PROFILE_FILE; we set GITAUR so the suite invokes the instrumented
# binary rather than the default target/debug/gaur path (and lib.sh resolves the
# example drivers next to it). This is the CI Tier-2 gate — a failure here fails
# the job (overall_status below), so both tiers gate merges.
set +e
GITAUR="/work/target/coverage-build/debug/gaur" \
    bash tests/container/run.sh --coverage "$PROFRAW_PODMAN" all
[[ $? -eq 0 ]] || { echo "scripts/coverage.sh: podman tests failed" >&2; overall_status=1; }
set -e

# Step 3 — three reports. `cargo llvm-cov report` only discovers profraw that
# sit *flat* in CARGO_LLVM_COV_TARGET_DIR (it does not recurse into subdirs and
# has no --profraw-dir flag). The rust and podman profraw were collected into
# profraw/{rust,podman}/ staging subdirs; for each report we copy just the
# group(s) we want flat into $DIR, run report, then clear the flat copies. The
# `report` binary runs as root, so it can read the podman profraw even though
# they are owned by the test container's subuid.
in_image '
    eval "$(cargo llvm-cov show-env --export-prefix)"
    DIR="$CARGO_LLVM_COV_TARGET_DIR"
    REGEX='"'$IGNORE_REGEX'"'
    mkdir -p coverage

    clear_flat() { find "$DIR" -maxdepth 1 \( -name "*.profraw" -o -name "*.profdata" \) -delete 2>/dev/null || true; }

    stage() {  # $1 = staging subdir under profraw/, $2 = flat filename prefix
        local f
        for f in "$DIR/profraw/$1"/*.profraw; do
            [[ -e "$f" ]] || continue
            cp "$f" "$DIR/$2-$(basename "$f")"
        done
    }

    report() {  # $1 = group name; remaining args = staging subdirs to include
        local group="$1"; shift
        clear_flat
        local sub
        for sub in "$@"; do stage "$sub" "$sub"; done
        cargo llvm-cov report --lcov \
            --output-path "coverage/lcov-$group.info" \
            --ignore-filename-regex "$REGEX"
        cargo llvm-cov report --ignore-filename-regex "$REGEX" \
            > "coverage/summary-$group.txt"
        clear_flat
    }

    report rust     rust
    report podman   podman
    report combined rust podman
'

echo
echo "=== Coverage summaries ==="
for g in rust podman combined; do
    echo
    echo "--- $g ---"
    cat "coverage/summary-$g.txt" 2>/dev/null || echo "(missing)"
done
echo
echo "lcov files in $REPO_ROOT/coverage/"

exit "$overall_status"
