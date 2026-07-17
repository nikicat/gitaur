#!/usr/bin/env bash
# Build the README demo screencasts (docs/plans/screencasts.md, phase 2).
#
# For each demo, run its `examples/demo_*.rs` driver inside the test container
# with cast recording on, then render the cast to docs/demo/<name>.gif with
# the image's pinned agg + fonts. Everything here is what CI would run — CI
# stays a thin wrapper over this script (the plan's reproducibility rule).
# Regenerated GIFs are never byte-identical (timing jitter); commit them at
# release cadence, not per UI tweak.
#
# Usage:
#   demos/build.sh                  # all demos
#   demos/build.sh search-install   # just one
#
# Outputs:
#   docs/demo/<name>.gif        (committed, embedded in README.md)
#   target/demo-casts/<name>.cast  (inspect with `asciinema play`)

set -euo pipefail

CONTAINER="${CONTAINER:-podman}"
IMAGE="aurox-test:latest"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# name → examples/demo_<name//-/_>.rs
ALL_DEMOS=(search-install)

demos=("$@")
[[ ${#demos[@]} -gt 0 ]] || demos=("${ALL_DEMOS[@]}")

# Always rebuild the image (cache makes this a no-op when nothing changed):
# the render toolchain is baked into it, so a stale image silently renders
# with old fonts/agg — the exact drift this script exists to prevent.
"$CONTAINER" build -t "$IMAGE" -f "$REPO_ROOT/tests/container/Dockerfile" \
    "$REPO_ROOT/tests/container"

( cd "$REPO_ROOT" && cargo build --bin aurox --examples )

# Same flat/777 layout as run.sh --record, same rootless-podman reasoning.
casts_dir="$REPO_ROOT/target/demo-casts"
mkdir -p "$casts_dir" "$REPO_ROOT/docs/demo"
chmod 777 "$casts_dir"
rm -f "$casts_dir"/*.cast

# Pins the built packages' .PKGINFO builddate (reproducible artifacts);
# makepkg's *console* dates still show wall clock — that's expected, and the
# future transcript normalization scrubs them (see the plan doc).
SDE=1782864000  # 2026-07-01 00:00:00 UTC

for name in "${demos[@]}"; do
    driver="demo_${name//-/_}"
    echo ":: recording $name ($driver)"
    "$CONTAINER" run --rm \
        -v "$REPO_ROOT:/work:ro" \
        -v "$(mktemp -d):/tmp/target" \
        -v "$casts_dir:/casts" \
        -e "PTY_CAST_DIR=/casts" \
        -e "PTY_CAST_NAME=$name" \
        -e "SOURCE_DATE_EPOCH=$SDE" \
        "$IMAGE" \
        bash -c "set -e; source /work/tests/container/lib.sh; bootstrap; reset_state; \
                 sudo sed -i 's/^#Color/Color/' /etc/pacman.conf; \
                 aurox -Sy; assert_exit 0; \"\$EXAMPLES_DIR/$driver\""
        # The sed turns on pacman's Color for this demo container only — the
        # image default (Arch's commented-out Color) stays plain for the test
        # suite, whose assertions grep uncolored pacman output.

    echo ":: rendering $name"
    # --user 0:0 so the GIF comes back owned by the host user (rootless
    # podman maps container root to the invoking user — coverage.sh's report
    # containers use the same trick).
    "$CONTAINER" run --rm --user 0:0 \
        -v "$casts_dir:/casts" \
        -v "$REPO_ROOT/docs/demo:/out" \
        "$IMAGE" \
        agg --font-family "JetBrains Mono" --font-size 16 --idle-time-limit 2 \
            "/casts/$name.cast" "/out/$name.gif"
done

echo
for name in "${demos[@]}"; do
    ls -la "$REPO_ROOT/docs/demo/$name.gif" "$casts_dir/$name.cast"
done
