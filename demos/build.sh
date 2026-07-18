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

# name → examples/demo_<name//-/_>.rs; a demos/seed-<name>.sh, if present, is
# sourced in the record container before the driver (outdated installs etc.).
# The demo set + titles live in one registry, demos/demos.json; the CI check
# run and the media-repo player pages consume the very same file (published
# per-dir as manifest.json), so adding a demo is a one-file edit — no list to
# keep in sync here, in the workflow, or in the player dropdowns.
mapfile -t ALL_DEMOS < <(jq -r '.[][0]' "$REPO_ROOT/demos/demos.json")

demos=("$@")
[[ ${#demos[@]} -gt 0 ]] || demos=("${ALL_DEMOS[@]}")

# Always rebuild the image (cache makes this a no-op when nothing changed):
# the render toolchain is baked into it, so a stale image silently renders
# with old fonts/agg — the exact drift this script exists to prevent.
"$CONTAINER" build -t "$IMAGE" -f "$REPO_ROOT/tests/container/Dockerfile" \
    "$REPO_ROOT/tests/container"

# Build aurox + the demo drivers INSIDE the test image, not on the host: the
# host (a CI runner especially) may lack libalpm, which alpm-sys must link, so
# the build has to happen where the Arch userspace lives. A dedicated target
# dir keeps these container-built artifacts from mixing with a dev's host
# `target/debug` (different sysroot paths would force churn). `--user 0:0`:
# rootless podman maps container root to the host user, so the artifacts come
# back writable and correctly owned (coverage.sh uses the same trick).
"$CONTAINER" run --rm --user 0:0 \
    -v "$REPO_ROOT:/work:rw" \
    -e CARGO_HOME=/work/target/demo-cargo \
    -e CARGO_TARGET_DIR=/work/target/demo-build \
    -w /work \
    "$IMAGE" \
    cargo build --bin aurox --examples
AUROX="/work/target/demo-build/debug/aurox"

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
        -e "AUROX=$AUROX" \
        "$IMAGE" \
        bash -c "set -e; source /work/tests/container/lib.sh; bootstrap; reset_state; \
                 sudo sed -i 's/^#Color/Color/' /etc/pacman.conf; \
                 if [[ -f /work/demos/seed-$name.sh ]]; then source /work/demos/seed-$name.sh; fi; \
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

    # Plain-text transcript for the base-vs-PR diff view (pushed to the media
    # repo, never committed — see docs/plans/screencasts.md). asciinema renders
    # the cast in-image; sed scrubs the per-run wall-clock noise on the host.
    "$CONTAINER" run --rm --user 0:0 -v "$casts_dir:/casts" "$IMAGE" \
        asciinema convert -f txt "/casts/$name.cast" - \
      | sed -E -f "$REPO_ROOT/demos/transcript-scrub.sed" > "$casts_dir/$name.txt"
done

echo
for name in "${demos[@]}"; do
    ls -la "$REPO_ROOT/docs/demo/$name.gif" "$casts_dir/$name.cast"
done
