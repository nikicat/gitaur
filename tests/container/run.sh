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

set -euo pipefail

CONTAINER="${CONTAINER:-podman}"
IMAGE="gitaur-test:latest"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TESTS_DIR="$REPO_ROOT/tests/container"

rebuild=0
jobs="$(nproc)"
selectors=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --rebuild) rebuild=1 ;;
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

# Build the binary on the host once, mount /work read-only.
( cd "$REPO_ROOT" && cargo build --bin gitaur )

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
results_dir="$(mktemp -d)"
trap 'rm -rf "$results_dir"' EXIT

run_one() {
    local script="$1"
    local rel="${script#$REPO_ROOT/}"
    local slug="${rel//\//_}"
    local out="$results_dir/$slug.out"
    if "$CONTAINER" run --rm \
            -v "$REPO_ROOT:/work:ro" \
            -v "$(mktemp -d):/tmp/target" \
            "$IMAGE" \
            bash -c "set -e; cd /work && bash $rel" >"$out" 2>&1; then
        echo "PASS $rel"
    else
        { echo "FAIL $rel"; sed 's/^/    /' "$out"; }
    fi
}
export -f run_one
export CONTAINER IMAGE REPO_ROOT results_dir

pass=0 fail=0
printf '%s\n' "${scripts[@]}" \
    | xargs -P "$jobs" -I {} bash -c 'run_one "$@"' _ {} \
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
[[ "$fail" -eq 0 ]]
