#!/usr/bin/env bash
# Drive the gitaur container test suite.
#
# Usage:
#   tests/container/run.sh                  # smoke tier only
#   tests/container/run.sh smoke            # explicit
#   tests/container/run.sh extended         # long tail
#   tests/container/run.sh all              # both tiers
#   tests/container/run.sh smoke/05_*.sh    # one or more specific scripts
#
# Engine: podman by default, override with CONTAINER=docker.
# Image is cached as `gitaur-test:latest`; rebuild with --rebuild.
# Parallelism: -j N (default = $(nproc), 1 disables, all tests are
# fully isolated by container so contention is on host CPU/IO only).
#
# Coverage mode:
#   --coverage <dir>   Bind-mount <dir> into each test container at /profraw
#                      and set LLVM_PROFILE_FILE so the gitaur binary writes
#                      LLVM source-coverage data there. Also skips the host
#                      `cargo build` step (the caller is expected to have
#                      already built an instrumented binary and to set
#                      GITAUR=<path-inside-/work>). Driven by scripts/coverage.sh.

set -euo pipefail

CONTAINER="${CONTAINER:-podman}"
IMAGE="gitaur-test:latest"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TESTS_DIR="$REPO_ROOT/tests/container"

rebuild=0
jobs="$(nproc)"
coverage_dir=""
selectors=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --rebuild) rebuild=1 ;;
        --coverage) coverage_dir="$2"; shift ;;
        -j) jobs="$2"; shift ;;
        -j*) jobs="${1#-j}" ;;
        smoke|extended|all) selectors+=("$1") ;;
        smoke/*|extended/*) selectors+=("$1") ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done
[[ ${#selectors[@]} -gt 0 ]] || selectors=(smoke)

# Build image if absent or --rebuild. Fixture baking is in this build, so a
# fixture change requires --rebuild.
if [[ "$rebuild" == "1" ]] || ! "$CONTAINER" image exists "$IMAGE" 2>/dev/null; then
    "$CONTAINER" build -t "$IMAGE" -f "$TESTS_DIR/Dockerfile" "$TESTS_DIR"
fi

# Build the binary on the host once, mount /work read-only. In coverage mode
# the orchestrator has already produced an instrumented binary, so we skip.
if [[ -z "$coverage_dir" ]]; then
    # `tarpit` is the HTTP-stall example used by the idle-timeout test in
    # extended/. Building it alongside gitaur is cheap and keeps the test
    # script container-side (no host cargo inside the container).
    ( cd "$REPO_ROOT" && cargo build --bin gitaur --example tarpit )
else
    mkdir -p "$coverage_dir"
fi

# Resolve selectors into a flat list of test scripts.
resolve() {
    case "$1" in
        all)      find "$TESTS_DIR"/{smoke,extended} -name '*.sh' -type f | sort ;;
        smoke)    find "$TESTS_DIR/smoke"    -name '*.sh' -type f | sort ;;
        extended) find "$TESTS_DIR/extended" -name '*.sh' -type f | sort ;;
        *)        ls "$REPO_ROOT/tests/container/$1" 2>/dev/null || true ;;
    esac
}
scripts=()
for s in "${selectors[@]}"; do
    while IFS= read -r f; do scripts+=("$f"); done < <(resolve "$s")
done
[[ ${#scripts[@]} -gt 0 ]] || { echo "no tests matched"; exit 2; }

# Each test runs in its own fresh container so state is isolated. With -j>1
# they run concurrently — each container has its own pacman DB / state dir
# so there is no cross-test contention.
#
# Captured-output files (one per script) are written to $results_dir. We
# keep that directory around on failure so a user can `cat` an individual
# log even if the inline dump was clipped by their terminal; on success we
# clean it up via the trap below.
results_dir="$(mktemp -d)"
keep_results=0
trap '[[ "$keep_results" == "1" ]] || rm -rf "$results_dir"' EXIT

run_one() {
    local script="$1"
    local rel="${script#$REPO_ROOT/}"
    local slug="${rel//\//_}"
    local out="$results_dir/$slug.out"

    # Coverage args, built from scalar env vars exported below (bash arrays
    # don't survive the xargs/bash -c bounce).
    local cov_args=()
    if [[ -n "${COVERAGE_DIR:-}" ]]; then
        # :U asks podman to chown the bind-mount to the in-container UID
        # (`builder`) so the unprivileged test process can write profraw.
        cov_args=(
            -v "$COVERAGE_DIR:/profraw:rw,U"
            -e "LLVM_PROFILE_FILE=/profraw/gitaur-%p-%m.profraw"
        )
        [[ -n "${GITAUR:-}" ]] && cov_args+=(-e "GITAUR=$GITAUR")
    fi

    if "$CONTAINER" run --rm \
            -v "$REPO_ROOT:/work:ro" \
            -v "$(mktemp -d):/tmp/target" \
            "${cov_args[@]}" \
            "$IMAGE" \
            bash -c "set -e; cd /work && bash $rel" >"$out" 2>&1; then
        echo "PASS $rel"
    else
        # Print the captured-output path explicitly so the user can re-read
        # it after the run finishes, and indent the body so the boundary
        # between FAIL header and captured lines is unambiguous. An empty
        # `$out` (e.g. when podman exits before producing output) shows up
        # as a clearly empty body, which is itself diagnostic.
        local size
        size=$(wc -c <"$out" 2>/dev/null || echo 0)
        echo "FAIL $rel  ($size bytes captured at $out)"
        if [[ "$size" -gt 0 ]]; then
            sed 's/^/    /' "$out"
        else
            echo "    <no output captured — likely container crash or silent set -e abort>"
        fi
    fi
}
export -f run_one
export CONTAINER IMAGE REPO_ROOT results_dir
export COVERAGE_DIR="$coverage_dir"
export GITAUR="${GITAUR:-}"

pass=0 fail=0
# `stdbuf -oL` flushes xargs's stdout per line so the FAIL+body block reaches
# the `while` reader without being held in a parallel-job buffer.
printf '%s\n' "${scripts[@]}" \
    | stdbuf -oL xargs -P "$jobs" -I {} bash -c 'run_one "$@"' _ {} \
    | while IFS= read -r line; do
        echo "$line"
        case "$line" in
            PASS*) pass=$((pass+1)) ;;
            FAIL*) fail=$((fail+1)) ;;
        esac
        # Persist counters across pipe via tempfile (bash subshell loses vars).
        echo "$pass $fail" > "$results_dir/.counters"
    done

read -r pass fail < "$results_dir/.counters"
echo
echo "== $pass passed, $fail failed =="
if [[ "$fail" -gt 0 ]]; then
    keep_results=1
    echo "captured logs preserved in $results_dir"
fi
[[ "$fail" -eq 0 ]]
