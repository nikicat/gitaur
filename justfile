# Project recipes. Run `just` to list, `just <recipe>` to invoke.
#
# Coverage uses cargo-llvm-cov (LLVM source-based coverage).
# Install once:  cargo install cargo-llvm-cov
# Or via pacman: pacman -S cargo-llvm-cov

# Filename regex passed to llvm-cov to drop CLI glue + test helpers from the
# report. We measure the *library*: src/main.rs is a thin wrapper around
# cli::run, and src/testing.rs is the shared #[doc(hidden)] fixture module
# consumed by tests/ (see src/lib.rs).
ignore_regex := '(examples/|src/main\.rs|src/testing\.rs)'

# List available recipes.
default:
    @just --list

# Run the full test suite (matches CI).
test:
    cargo test --all-features --locked

# Cut a release: bump the version on a branch, open a PR, wait for its CI,
# and merge — the merge to main IS the release (release.yml tags the merge
# commit, creates the GitHub release, test-builds the PKGBUILD, and publishes
# to the AUR). Nothing is pushed to main directly, so this works with a
# protected main; the PR's CI run gates the merge, and a failed run leaves
# the branch + PR in place to inspect. `bump` is patch|minor|major or an
# explicit version like 0.2.0. Cargo.lock must carry the new version too:
# the PKGBUILD builds with --frozen, so a stale lock fails the release build.
release bump='patch':
    #!/usr/bin/env bash
    set -euo pipefail
    [ "$(git symbolic-ref --short HEAD)" = main ] || { echo 'not on main' >&2; exit 1; }
    [ -z "$(git status --porcelain)" ] || { echo 'working tree not clean' >&2; exit 1; }
    git fetch origin main
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
        || { echo 'main is not in sync with origin/main' >&2; exit 1; }
    cur=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
    IFS=. read -r maj min pat <<<"$cur"
    case '{{bump}}' in
        major) new="$((maj + 1)).0.0" ;;
        minor) new="$maj.$((min + 1)).0" ;;
        patch) new="$maj.$min.$((pat + 1))" ;;
        *) new='{{bump}}'
           [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
               || { echo 'bump must be patch|minor|major or X.Y.Z' >&2; exit 1; } ;;
    esac
    ! git ls-remote --exit-code --tags origin "refs/tags/v$new" >/dev/null 2>&1 \
        || { echo "v$new is already released" >&2; exit 1; }
    read -rp "release v$new (current: v$cur)? [y/N] " answer
    [[ "$answer" == [yY]* ]] || { echo 'aborted'; exit 1; }
    git switch -c "release-v$new"
    sed -i "0,/^version = \".*\"/s//version = \"$new\"/" Cargo.toml
    cargo update --workspace
    git add Cargo.toml Cargo.lock
    git commit -m "Bump version to $new"
    git push -u origin "release-v$new"
    gh pr create --base main --title "Bump version to $new" \
        --body "Merging this PR releases v$new: release.yml tags the merge commit, creates the GitHub release, and publishes to the AUR."
    # Right after the push, CI may not have reported its check yet, and
    # `gh pr checks` treats "no checks" as an error (exit 1) rather than
    # something to wait for — poll until a check exists (0 passed/8 pending),
    # then let --watch do the real blocking.
    for _ in $(seq 20); do
        rc=0; gh pr checks >/dev/null 2>&1 || rc=$?
        [ "$rc" = 1 ] || break
        sleep 3
    done
    gh pr checks --watch --fail-fast
    gh pr merge --merge --delete-branch
    git pull --ff-only origin main
    echo "merged — release.yml takes it from here:"
    echo "https://github.com/nikicat/aurox/actions/workflows/release.yml"

# Republish an existing release tag to the AUR with the current PKGBUILD.in
# (e.g. after a template fix) — creates no new tag or GitHub release.
republish tag:
    gh workflow run release.yml --field tag={{tag}}

# Coverage summary in the terminal.
coverage:
    cargo llvm-cov --all-features --ignore-filename-regex '{{ignore_regex}}'

# HTML report at target/llvm-cov/html/index.html.
coverage-html:
    cargo llvm-cov --all-features --html --ignore-filename-regex '{{ignore_regex}}'

# HTML report + open in browser.
coverage-open:
    cargo llvm-cov --all-features --html --open --ignore-filename-regex '{{ignore_regex}}'

# lcov.info for Codecov upload or external tools.
coverage-lcov:
    cargo llvm-cov --all-features --lcov --output-path lcov.info \
        --ignore-filename-regex '{{ignore_regex}}'

# Drop cached .profraw / .profdata / HTML report.
coverage-clean:
    cargo llvm-cov clean --workspace

# The podman test containers write profraw as an unprivileged subuid (via the
# :U mount), which the host user can't rm directly — so try `podman unshare`
# first and fall back to a plain rm (e.g. when using rootful docker).
# Drop the container coverage build dir, cargo cache, and lcov outputs.
coverage-all-clean:
    podman unshare rm -rf target/coverage-build 2>/dev/null || rm -rf target/coverage-build
    rm -rf target/coverage-cargo coverage

# Runs everything inside the test image (needs only podman/docker on the host),
# mirroring the coverage job in .github/workflows/ci.yml; writes
# coverage/lcov-{rust,podman,combined}.info plus summaries.
# Three-tier coverage: rust tests, podman container tests, and combined.
coverage-all:
    bash scripts/coverage.sh
