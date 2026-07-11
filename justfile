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
# the branch + PR in place to inspect. After the merge the recipe watches the
# Release run through the AUR push and confirms the AUR serves the new
# version. `bump` is patch|minor|major or an
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
    pr_url=$(gh pr create --base main --title "Bump version to $new" \
        --body "Merging this PR releases v$new: release.yml tags the merge commit, creates the GitHub release, and publishes to the AUR.")
    echo "$pr_url"
    # Right after the push, CI may not have reported its check yet, and
    # `gh pr checks` treats "no checks" as an error (exit 1) rather than
    # something to wait for — poll until a check exists (0 passed/8 pending),
    # then let --watch do the real blocking. --required scopes both calls to
    # the checks branch protection actually enforces: informational statuses
    # (codecov project drift) failed spuriously on the v0.1.3 bump PR and
    # aborted the release mid-flight, even though the merge was allowed.
    for _ in $(seq 20); do
        rc=0; gh pr checks --required >/dev/null 2>&1 || rc=$?
        [ "$rc" = 1 ] || break
        sleep 3
    done
    gh pr checks --required --watch --fail-fast
    gh pr merge --merge --delete-branch
    git pull --ff-only origin main
    merge_sha=$(gh pr view "$pr_url" --json mergeCommit --jq .mergeCommit.oid)
    just _watch-release "v$new" "$merge_sha"

# Republish an existing release tag to the AUR with the current PKGBUILD.in
# (e.g. after a template fix) — creates no new tag or GitHub release — then
# watch the run and confirm the AUR picked the version up.
publish tag:
    #!/usr/bin/env bash
    set -euo pipefail
    git fetch origin main
    gh workflow run release.yml --field tag='{{tag}}'
    # The dispatched run executes on main's head; filtering the run lookup by
    # that sha (plus the event) keeps the watcher off older dispatch runs.
    just _watch-release '{{tag}}' "$(git rev-parse origin/main)" workflow_dispatch

# Find the Release run for a commit (and optionally trigger event), watch it
# to completion, then poll the AUR until it serves the released version.
# cgit reflects the push immediately; the RPC endpoint lags a minute or two.
_watch-release tag commit event='':
    #!/usr/bin/env bash
    set -euo pipefail
    sel=(--workflow=release.yml --commit '{{commit}}' --limit 1)
    [ -z '{{event}}' ] || sel+=(--event '{{event}}')
    echo 'waiting for the Release run to appear…'
    run_id=
    for _ in $(seq 30); do
        run_id=$(gh run list "${sel[@]}" --json databaseId --jq '.[0].databaseId // empty')
        [ -n "$run_id" ] && break
        sleep 5
    done
    [ -n "$run_id" ] || { echo 'no Release run appeared after 150s' >&2; exit 1; }
    gh run watch "$run_id" --exit-status \
        || { echo "Release run failed — inspect: gh run view $run_id --log-failed" >&2; exit 1; }
    ver='{{tag}}'; ver="${ver#v}"
    echo "run succeeded — waiting for the AUR to serve aurox $ver…"
    for _ in $(seq 30); do
        aur=$(curl -sf 'https://aur.archlinux.org/cgit/aur.git/plain/.SRCINFO?h=aurox' \
            | sed -n 's/^[[:space:]]*pkgver = //p' || true)
        [ "$aur" = "$ver" ] && { echo "AUR serves aurox $aur — release complete"; exit 0; }
        sleep 10
    done
    echo "AUR still serves '${aur:-?}' after 5 min; expected $ver" >&2
    exit 1

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
