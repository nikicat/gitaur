# Testing gitaur

Two tiers. Both are required to pass before merging.

## Tier 1 — Cargo unit + integration tests

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
binary that links the gitaur lib. `tests/testing.rs` (re-exported via
`gitaur::testing`) is a shared helper module that wraps the system
`git` CLI for fixture setup, since gix doesn't expose every plumbing
operation we need.

**When to add a Tier-1 test:**
- The function is pure data → put the test in the same file under
  `#[cfg(test)] mod tests`.
- The function needs disk state (a real bare repo, a tempdir, etc.) but
  doesn't shell out to `pacman` / `makepkg` → put it under `tests/`.
- Anything that needs an `alpm` handle or a real `pacman` → Tier 2.

## Tier 2 — Container integration suite

Real Arch userspace. Runs every gitaur command path against real
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
├── lib.sh                   bash helpers: gitaur, assert_*, bootstrap, reset_state
├── run.sh                   the harness — parallel containers + xargs -P
├── fixtures/<pkgbase>/      one PKGBUILD per fixture, optional .install / repo
├── smoke/NN_*.sh            ~20 everyday cases (always run in CI)
└── extended/                long tail (rare combos, edge cases)
    └── .scope               planned-test index — see file for the list
```

Each smoke test is a self-contained bash script:

```bash
#!/usr/bin/env bash
source /work/tests/container/lib.sh
bootstrap; reset_state

gitaur -Sy
gitaur -S --noconfirm test-trivial
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
- `repo` — optional, one word: `aur` (default) or `official`.

`official` fixtures get built once at image build time and registered
into `/srv/local-repo` (a real pacman sync DB). `aur` fixtures get
published as a `refs/heads/<pkgbase>` branch in `/srv/mock-aur` (a bare
git repo that mimics `github.com/archlinux/aur`'s layout exactly).

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
gitaur <args...>              # runs $GITAUR with args; captures stdout/stderr/exit
LAST_STDOUT / LAST_STDERR     # captured-output file paths
LAST_EXIT                     # captured exit code
assert_exit N
assert_stdout_contains "..."  # substring (literal)
assert_stderr_contains "..."
assert_pkg_installed <name>
assert_pkg_not_installed <name>
assert_pkg_explicit <name>    # pacman Install Reason = Explicitly installed
assert_pkg_asdep <name>       # pacman Install Reason = as a dependency
reset_state                   # wipe ~/.local/state/gitaur between phases
```

### Debugging a failing container test

```sh
# Run one test verbosely:
podman run --rm -v "$PWD:/work:ro" -v "$(mktemp -d):/tmp/target" \
    gitaur-test:latest \
    bash -xc "cd /work && bash tests/container/smoke/05_aur_with_aur_dep.sh"

# Interactive shell in the test image, fixtures already baked:
podman run --rm -it -v "$PWD:/work:ro" -v "$(mktemp -d):/tmp/target" \
    gitaur-test:latest bash
```

Inside the container the gitaur binary is at `/work/target/debug/gitaur`,
`RUST_LOG=gitaur=debug` is the helpful verbosity level.

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

## Manual smoke tests against the real AUR

The unit + container suites use synthetic fixtures by design, so they
exercise the code paths but not the messy shape of real-world AUR data.
For sanity-checking changes to `resolver/` or to plan-rendering, run
`--plan` against representative pkgbases from a populated index
(`gitaur -Sy` first). `--plan` resolves the full Plan and prints
strata without touching `makepkg` or `pacman`, so it's safe to run
freely.

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
it after `gitaur -Sy` to refresh the candidates if the index drifts:

```sh
cargo run --example deep_strata
```

It uses the real `resolver::resolve`, so anything it reports is
something gitaur can actually plan.

## CI

Tier 1 runs in GitHub Actions on every push/PR via `.github/workflows/ci.yml`.
Tier 2 has higher startup cost (image build) so we run it on a separate
workflow (TODO when this lands in CI). Until then: run it locally before
merging non-trivial changes to `resolver/`, `build/`, or `pacman/`.

## When tests fail

Per project rule (`memory/feedback_*`):

- **No workarounds in tests.** If the test reveals a bug in gitaur, fix
  gitaur. Don't paper over with `|| true` or skip-this-for-now sentinels.
- **Trace through the cause.** Most container failures so far have been
  real gitaur bugs surfacing for the first time, not test setup issues.
  Don't assume the test is wrong before checking the binary's behaviour.

## Future work for tier 2

The `extended/` tier is mostly empty stubs in `.scope`. The next ones
worth adding (in roughly priority order):

- `epoch_dominates_version.sh`
- `vcs_pkg_skipped_without_devel.sh` / `…picked_up_with_devel.sh`
- `install_hook_runs.sh`
- `provides_virtual_resolves.sh`
- `cycle_in_aur_deps_errors.sh` (we have one for cycle-makedep already)
- `rebuild_cached_skips.sh` (state.db idempotency)
- `mirror_unreachable.sh` (rm /srv/mock-aur mid-test)

Pick from `tests/container/extended/.scope` for the full backlog.
