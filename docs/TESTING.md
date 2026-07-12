# Testing aurox

Two layers. Both are required to pass before merging.

## Cargo unit + integration tests

Pure Rust. Run anywhere with a working `cargo`. Fast (sub-second once
the workspace is built).

```sh
cargo test          # unit + integration tests
cargo clippy --all-targets -- -D warnings
```

What lives where:

| Layer                | Test location                              | Tests for |
| -------------------- | ------------------------------------------ | --------- |
| Pure data            | `#[cfg(test)] mod tests` next to the code  | parsers, formatters, schema, classifiers, Plan structure |
| In-process I/O       | `tests/*.rs`                               | full mirror fetch + index build + incremental update |

The `tests/` directory holds true integration tests: each is a separate
binary that links the aurox lib. `tests/testing.rs` (re-exported via
`aurox::testing`) is a shared helper module that wraps the system
`git` CLI for fixture setup, since gix doesn't expose every plumbing
operation we need.

**When to add a cargo test:**
- The function is pure data → put the test in the same file under
  `#[cfg(test)] mod tests`.
- The function needs disk state (a real bare repo, a tempdir, etc.) but
  doesn't shell out to `pacman` / `makepkg` → put it under `tests/`.
- Anything that needs an `alpm` handle or a real `pacman` → the container
  suite.

## Container integration suite

Real Arch userspace. Runs every aurox command path against real
`makepkg`, real `pacman -S/-U/-D`, and real `alpm` — but with fixture
PKGBUILDs designed to build in well under a second.

```sh
bash tests/container/run.sh                       # smoke tier
bash tests/container/run.sh extended              # rare combos
bash tests/container/run.sh all                   # both
bash tests/container/run.sh -j 1 smoke            # serial
bash tests/container/run.sh --rebuild smoke       # bust image cache
CONTAINER=docker bash tests/container/run.sh      # docker instead of podman
```

Image build: ~2 min one-time. Smoke suite: ~30 s on 8 cores.

### Layout

```
tests/container/
├── Dockerfile               base-devel + builder user + baked fixtures
├── setup-fixtures.sh        builds repo-* pkgs, publishes AUR-* branches
├── lib.sh                   bash helpers: aurox, assert_*, bootstrap, reset_state
├── run.sh                   the harness — parallel containers + xargs -P
├── fixtures/<pkgbase>/      one PKGBUILD per fixture, optional .install / repo
├── smoke/NN_*.sh            everyday cases (run in CI)
└── extended/                long tail (rare combos, edge cases; also run in CI)
    └── .scope               planned-test index — see file for the list
```

Each smoke test is a self-contained bash script:

```bash
#!/usr/bin/env bash
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
aurox -S --noconfirm test-trivial
assert_exit 0
assert_pkg_installed test-trivial
assert_pkg_explicit  test-trivial
```

### Why containers, not mocks like paru's `feature = "mock"`?

`paru` mocks `pacman` / `makepkg` shims on `PATH` and asserts on a fake
alpm DB. That's faster but misses the interface drift that broke us
twice already (`gix::prepare_clone_bare` refspec, missing
`alpm-utils::alpm_with_conf` call). Real Arch userspace catches these.

### How fixtures are designed

A fixture is a single directory under `fixtures/` with:

- `PKGBUILD` — required.
- `<name>.install` — optional install hook.
- `repo` — optional, one word: `aur` (default), `official`, or `foreign`.

`official` fixtures get built once at image build time and registered
into `/srv/local-repo` (a real pacman sync DB). `aur` fixtures get
published as a `refs/heads/<pkgbase>` branch in `/srv/mock-aur` (a bare
git repo that mimics `github.com/archlinux/aur`'s layout exactly).
`foreign` fixtures are built once and the `.pkg.tar.zst` staged under
`/srv/foreign-pkgs/`; they are **not** registered in any sync repo and
**not** mirrored into the AUR. A test seeds them with `install_foreign
<pkgbase>` (a `lib.sh` helper that wraps `sudo pacman -U`) to create the
foreign-install state — in pacman's localdb but absent from every
source — that the resolver's `by_provides` walk is designed for. Without
this class, a test targeting that path would route through `by_name`
instead and silently skip the hint plumbing under test (see
[`ARCHITECTURE.md`'s resolution case matrix](ARCHITECTURE.md#resolution-case-matrix),
row 8a).

Three design rules:

1. **Build in under a second.** Most fixtures are pure-metadata: a
   single `install -Dm644 /dev/null ...` in `package()`, no sources, no
   compilation. Even the "deep makedep chain" fixtures use `echo`
   scripts. Total fixture-bake time at image build is ~10 s for ~25
   pkgs.
2. **Exercise the gnarly bit, not the happy path.** A trivial-bin PKGBUILD
   is fine for the basic install test; for split packages we use one
   with two `package_*()` functions; for makedep chains the `build()`
   actually invokes the dep's binary so it provably has to be installed
   first.
3. **No network.** Sources, if any, are `file://` — but most fixtures
   have empty `source=()`.

### Adding a new fixture

```sh
mkdir tests/container/fixtures/test-mything
cat > tests/container/fixtures/test-mything/PKGBUILD <<'EOF'
pkgname=test-mything
pkgver=1.0
pkgrel=1
arch=('any')
license=('MIT')
package() {
    install -Dm644 /dev/null "$pkgdir/usr/share/$pkgname/marker"
}
EOF
bash tests/container/run.sh --rebuild smoke   # rebuilds image so the new pkg is baked
```

### Adding a new smoke / extended test

Copy a small existing one and adjust:

```sh
cp tests/container/smoke/01_install_repo_pkg.sh \
   tests/container/smoke/23_my_case.sh
chmod +x tests/container/smoke/23_my_case.sh
$EDITOR    tests/container/smoke/23_my_case.sh
bash tests/container/run.sh smoke/23_my_case.sh
```

Available helpers (defined in `lib.sh`):

```
aurox <args...>                # runs $AUROX with args; captures stdout/stderr/exit
LAST_STDOUT / LAST_STDERR     # captured-output file paths
LAST_EXIT                     # captured exit code
assert_exit N
assert_stdout_contains "..."  # substring (literal)
assert_stderr_contains "..."
assert_pkg_installed <name>
assert_pkg_not_installed <name>
assert_pkg_explicit <name>    # pacman Install Reason = Explicitly installed
assert_pkg_asdep <name>       # pacman Install Reason = as a dependency
install_foreign <pkgbase>     # sudo pacman -U /srv/foreign-pkgs/<pkgbase>-*.pkg.tar.zst
reset_state                   # wipe ~/.local/state/aurox between phases
```

### Debugging a failing container test

```sh
# Run one test verbosely:
podman run --rm -v "$PWD:/work:ro" -v "$(mktemp -d):/tmp/target" \
    aurox-test:latest \
    bash -xc "cd /work && bash tests/container/smoke/05_aur_with_aur_dep.sh"

# Interactive shell in the test image, fixtures already baked:
podman run --rm -it -v "$PWD:/work:ro" -v "$(mktemp -d):/tmp/target" \
    aurox-test:latest bash
```

Inside the container the aurox binary is at `/work/target/debug/aurox`,
`RUST_LOG=aurox=debug` is the helpful verbosity level.

### Common pitfalls

- **`makepkg` refuses root.** The image's `builder` user owns
  `/srv/local-repo` and `/srv/mock-aur` so `setup-fixtures.sh` can run
  as builder. Don't `sudo` setup scripts.
- **Refs land under `refs/remotes/origin/*` by default.** gix's
  `prepare_clone_bare` matches non-bare `git clone` semantics, not
  `git clone --bare`. `src/mirror/clone.rs` overrides this — if you ever
  touch the clone path, run `cargo test --test clone_refs_layout` to
  confirm the regression test still catches it.
- **alpm sync DBs are empty by default.** `Alpm::new` doesn't register
  syncdbs from `pacman.conf` — `pacman::alpm_db::open` uses
  `alpm-utils::alpm_with_conf` to do that. If you reach for `Alpm::new`
  directly, you'll get an empty `syncdbs()` and every repo target will
  classify as Missing.
- **xargs counter aggregation in `run.sh`.** `pass`/`fail` counters are
  computed in a pipeline subshell; the final report reads back from a
  tempfile. Don't try to make it interactive — the parallel runner
  intentionally swallows stdin.

### Smoke tests for build resilience

`smoke/28-30` cover the per-pkgbase failure isolation introduced
alongside the makepkg log capture: independent pkgbases keep building
when a sibling fails, dependents of a failed pkgbase are auto-skipped
instead of attempted with a missing dep, and `<worktree>/build.log`
keeps a verbatim copy of every makepkg run for post-mortem use. The
failing-build fixtures (`test-fail-build`, `test-needs-fail`) exist
only to drive these three tests — they are baked into the image, so
adding a new test that touches them does not require `--rebuild`
unless you also edit the fixture's `PKGBUILD`.

## Manual smoke tests against the real AUR

The unit + container suites use synthetic fixtures by design, so they
exercise the code paths but not the messy shape of real-world AUR data.
For sanity-checking changes to `resolver/` or to plan-rendering, run
`aurox -S <pkgbase>` against representative entries from a populated
index (`aurox -Sy` first) and decline the `Proceed with installation?`
prompt — the resolver prints the full Plan up front, so `n` exits
cleanly without touching `makepkg` or `pacman`.

Candidates (verified against the May 2026 AUR mirror — pkgbase names
are stable, the exact dep counts drift):

| Pkgbase                  | Strata | AUR pkgs | What it exercises |
| ------------------------ | -----: | -------: | ----------------- |
| `spotify`                |      1 |        1 | single-stratum, mixed repo deps |
| `ptree`                  |      2 |        2 | two strata, no repo deps |
| `python-pythonnet`       |      2 |        2 | two strata, no repo deps |
| `ffmpeg-compat-54`       |      3 |        3 | three strata, heavy repo deps |
| `ros-melodic-move-base`  |     15 |       93 | deep stratum stack, broad repo deps |
| `ros-melodic-turtlebot3` |     15 |      121 | widest realistic AUR pipeline |

Avoid `python38-*` pkgbases: they reference ~60 missing dependencies
(`python3.8` itself, plus an orphaned helper-pkg tail), so the resolver
fails fast with `UnknownTargets` before producing a Plan. Useful as a
negative-test target for the missing-deps error path, but not for plan
rendering.

`examples/deep_strata.rs` is the scanner that produced this list — run
it after `aurox -Sy` to refresh the candidates if the index drifts:

```sh
cargo run --example deep_strata
```

It uses the real `resolver::resolve`, so anything it reports is
something aurox can actually plan.

## CI

Both layers run in GitHub Actions on every push/PR, in parallel jobs of
`.github/workflows/ci.yml`:

- **`ci`** — `fmt` + `taplo` + `clippy` + `build` + `cargo test`, on an
  `archlinux:latest` container.
- **`container tests + coverage`** — the container-suite gate. Runs the whole
  container suite (`smoke` + `extended`) via `scripts/coverage.sh` →
  `tests/container/run.sh --coverage … all` against an instrumented binary, so
  the same run also produces the rust/podman/combined coverage uploaded to
  Codecov. A container-test failure fails the job. (Coverage is the byproduct;
  running the tests is the point.) The in-image `cargo test` runs under a PTY
  (`script(1)`): color autodetection then picks the colored rendering, the
  condition an interactive `makepkg check()` creates — v0.1.2 shipped a test
  that only failed there. The `ci` job's piped run keeps the plain rendering
  covered.
- **`makepkg from tree`** — the packaging gate. Renders
  `packaging/aur/PKGBUILD.in` against a `git archive HEAD` tarball (makepkg
  skips the download when the source file is already present) and runs the full
  `makepkg` build + `check()` as an unprivileged user under a PTY. Catches what
  only the release-profile AUR build sees — missing `depends`, `--release`-only
  `check()` failures, `Cargo.lock` drift — *before* a release is cut; the
  release workflow's own test-build can only run once the tag exists.

Running the suite instrumented has a higher startup cost (it builds the test
image and an instrumented binary), which is why it's a job of its own rather
than folded into `ci`. You can still run it locally without any of the coverage
plumbing — `bash tests/container/run.sh` — which is faster and the right thing
to do before merging non-trivial changes to `resolver/`, `build/`, or `pacman/`.

## When tests fail

Check these two podman-suite quirks **first** — they cause the same
confusing failures over and over:

- **Stale image after a fixture change.** The container image bakes in the
  mock AUR + local repo from `fixtures/*/` at *image-build* time. `run.sh`
  recompiles and mounts the `aurox` binary every run, but only rebuilds the
  **image** on `--rebuild` (or when it's absent). So after editing a fixture
  (`PKGBUILD`/`repo`/`commit-date`), the `Dockerfile`, or `setup-fixtures.sh`,
  you **must**
  `tests/container/run.sh --rebuild`. Telltale symptom: a new fixture fails
  with `error: unknown target(s): <fixture>`, or a changed PKGBUILD silently
  runs its old contents. This is a stale image, **not** a binary/source
  bug — don't go editing Rust to chase it. Source-only changes never need
  `--rebuild`.
- **Parallelism flakes.** Default `-j $(nproc)`; under host contention a lone
  script can fail and then pass in isolation. Confirm with
  `tests/container/run.sh smoke/NN_*.sh` or `-j1` before treating it as real.

Per project rule (`memory/feedback_*`):

- **No workarounds in tests.** If the test reveals a bug in aurox, fix
  aurox. Don't paper over with `|| true` or skip-this-for-now sentinels.
- **Trace through the cause.** Most container failures so far have been
  real aurox bugs surfacing for the first time, not test setup issues.
  Don't assume the test is wrong before checking the binary's behaviour.

## Future work for the container suite

The `extended/` tier is mostly empty stubs in `.scope`. The next ones
worth adding (in roughly priority order):

- `epoch_dominates_version.sh`
- `vcs_pkg_skipped_without_devel.sh` / `…picked_up_with_devel.sh`
- `install_hook_runs.sh`
- `provides_virtual_resolves.sh`
- `cycle_in_aur_deps_errors.sh` (we have one for cycle-makedep already)
- `rebuild_cached_skips.sh` (artifact-cache idempotency — a pkgbase whose
  `.pkg.tar.*` is already on disk at the index version skips makepkg)
- `mirror_unreachable.sh` (rm /srv/mock-aur mid-test)

Pick from `tests/container/extended/.scope` for the full backlog.
