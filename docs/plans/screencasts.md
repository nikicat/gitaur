# Automated terminal screencasts

Two goals, one recorder:

1. **README demos** — newcomers should see aurox's flows (search+install,
   upgrade, remove, the shell) in seconds, without installing it.
2. **Test visibility** — scripted PTY e2e tests give no overview of what
   actually happened on screen; recordings make a red run *watchable*, the
   way Playwright videos do for web UI suites.

Everything below was validated empirically before being adopted (sizes,
emoji rendering, SOURCE_DATE_EPOCH behavior — see "Measurements").

## Architecture

One pipeline, two consumers. The container suite is the recording studio —
hermetic (mock AUR at `file:///srv/mock-aur`, local pacman repo, pinned
fixtures), so recordings are reproducible and never touch the network.

```
pty-harness spawn ──► asciicast v2 (.cast, ~15 KB)
      │                    │
      │ (tests)            │ (demos)
      ▼                    ▼
  CI artifact          agg ──► GIF (~1.4 MB / 25 s)
  (debug failed runs)  (README, PR review)
```

- **Recorder** (phase 1, done): `pty-harness/src/cast.rs` tees the reader
  thread's raw PTY bytes into an asciicast v2 file when `PTY_CAST_DIR` is
  set. Timing is captured at read time; a carry buffer keeps UTF-8 valid
  across the 8 KiB read boundary (a split emoji would otherwise corrupt two
  events). Best-effort by contract: recording failure warns and disables
  itself, never fails a scenario. `run.sh --record` wires it per test;
  `scripts/coverage.sh` passes `--record` so CI uploads `target/casts/` as
  the `pty-casts` artifact (`if: always()` — failures are when it matters).
- **Demo drivers** (phase 2): dedicated `examples/demo_*.rs` reusing
  `pty_harness` — not the assert-heavy test drivers — with two pacing
  helpers: `send_human` (char-by-char with small seeded jitter; rustyline's
  echo turns that into a real typing animation in the recording) and `dwell`
  (reading pauses). They run in the container against the same fixtures and
  double as extended-tier tests (assert clean exit), so demos can't rot.
- **Renderer**: `agg` (pinned release binary) + `ttf-jetbrains-mono` +
  `noto-fonts-emoji`, baked into the test image so renders don't depend on
  host tool versions. VHS was evaluated and rejected: it would add
  ttyd+ffmpeg+Chromium and a second scripting language for flows already
  scripted in Rust; agg renders the UI's `📥`/`🔨` emoji and `→` correctly
  (verified), which was the one thing that could have forced VHS.

## Reproducibility rules

- **CI is a thin wrapper.** Every CI step is a locally runnable command
  (`run.sh --record`, the future `demos/build.sh`). No CI-only logic.
- **"Reproducible" means same command, equivalent artifact** — never
  byte-identical (timing jitter). Recordings are review aids and debugging
  artifacts; they gate nothing and are never byte-compared.
- **`SOURCE_DATE_EPOCH` is set in the record environment.** It pins the
  built packages' `.PKGINFO` builddate (verified) — but *not* makepkg's
  console dates, which are scrubbed at transcript level instead (below).
  libfaketime was considered and dropped: LD_PRELOAD clock-faking risks
  wedging build tools (clock vs file-mtime skew), and nothing else needs it.

## Hosting & publication

Measured sizes drive the choices: a 25 s cast is 15 KB; a README-quality
GIF is ~1.4 MB at font-size 16 (100×30 grid).

- **README GIFs: committed to main**, regenerated at release cadence only
  (~3 MB per regen across 3 demos — acceptable for years). **Git LFS is
  ruled out**: GitHub does not render LFS-tracked images in READMEs (the
  raw URL serves the pointer file), and LFS bandwidth from public clones
  bills the repo owner.
- **Sidecar repo** (`aurox-ci-media` or similar): per-PR recordings under
  `pr-<n>/`, current main under `main/` (refreshed by an on-merge workflow —
  this is the comparison base, so base never needs rebuilding). Junk repo:
  prunable, force-pushable, zero clone-weight for aurox. Needs a
  deploy-key/PAT secret. Its GitHub Pages serves **asciinema-player**
  (self-contained JS): `play.html?cast=pr-123/upgrade.cast` gives full
  timeline scrubbing with no third-party service (asciinema.org rejected:
  external dependency, anonymous uploads expire). If README history bloat
  ever gets real, the same repo hosts the README GIFs via absolute raw
  URLs — that's the escape hatch, not LFS.

## PR surface

- **The `screencasts` check run carries everything** (on PRs touching
  `src/**`, `demos/**`, `examples/**`, `pty-harness/**`, fixtures, the
  Dockerfile): the Checks API `output` markdown (≤64 KB) holds the GIF
  gallery (embedded as markdown images — the `images[]` field is silently
  dropped by the API) plus the player, side-by-side, and text-diff links.
  Checks are SHA-bound, so every push keeps its own screencasts. Default
  `GITHUB_TOKEN` with `checks: write` suffices.
- **No PR comment, deliberately.** A GIF gallery in the conversation swamps
  the PR log with scrolling; the check run keeps it one click away in the
  Checks tab instead. (An earlier sticky-comment version was removed.)
- **Side-by-side** and **text diff** are Pages apps on the media repo
  (`compare.html` / `diff.html`), linked from the check run.

## Change detection: path filter + human-judged side-by-side

"Did the UI output change vs base?" — byte-diffing recordings is meaningless
(timing jitter), so the original plan was committed **keyframe transcripts**:
dump the normalized vt100 `screen()` at each quiescent point, commit them,
and let a `git diff` against base answer the question while a freshness gate
(regen is a no-op) keeps them honest.

**That approach was tried and dropped.** It needs the demo output to be
deterministic, and it isn't: the search/cli/repo demos deliberately show real
`[extra]` packages (texlive-games, nwg-hello, …) for realism, and those
versions and the result set move whenever `archlinux:latest` refreshes —
which `demos/build.sh` re-pulls on every run. A committed-transcript
freshness gate would flake on unrelated base-image updates. Scrubbing the
real-repo rows would blank exactly the content worth watching for change;
stripping real repos (as the upgrade seed does) would trade the demos' README
realism for determinism. Not worth it.

The move is to drop the *committed, gated* transcript but keep the transcript
*value* as a non-committed, human-read artifact — which sidesteps the whole
obstacle (nothing is committed, nothing gates the merge, so base-image
version drift is just diff noise a reviewer skims, exactly as in the GIF).
What ships:

- **The path filter is the change gate.** The whole Screencasts workflow only
  runs when UI-affecting files change (`src/**`, `demos/**`, `examples/**`,
  `pty-harness/**`, the fixtures, the Dockerfile). Coarser than a transcript —
  it can fire on a visually-neutral refactor — but robust, with zero flake.
- **The human judges "did it actually change"** two ways, both from the casts
  already stored in the media repo (`main/` = base, `pr-<N>/` = head — no base
  re-recording):
  - `compare.html?pr=<N>` — the base-vs-PR **side-by-side player**: two
    asciinema-players under one scrub bar, `main` beside the PR, frame for
    frame.
  - `diff.html?pr=<N>` — the base-vs-PR **text diff**: each cast rendered to
    plain text (`asciinema convert -f txt`), normalized by
    `demos/transcript-scrub.sed` (scrubs makepkg `(DATE)` stamps and
    `(AGE ago)` cells), then an LCS line diff `main` vs the PR. This is the
    "precise change detection" the original plan wanted — delivered as an
    ephemeral CI artifact instead of a committed snapshot, which is the part
    that makes it robust. It shows a `+adds/−dels` summary (or "no output
    change") but gates nothing.

Both links live on the PR's `screencasts` check run. Both self-explain the
bootstrap phase: `main/` (the base) is empty until the first screencast PR
merges, so for that first PR the pages show "no base on `main` yet — showing
the PR alone" rather than a bogus base-vs-PR view. A deterministic *gated*
transcript stays possible only if a fully-hermetic, fixture-only demo variant
is ever added (no real repos); until then a gate would flake, so it is out —
but the diff *view* above needs no such determinism.

## Demo set (each 15–30 s, 100 cols) — all recorded

1. **Hero** (top of README): `aurox hello` — banner, seeded search, stage,
   review-gate refusal → approve, apply with streaming build and sudo gate.
2. **cli-install**: `aurox -S test-hello` typed in a demo bash
   (`Pty::spawn_demo_shell`) — PKGBUILD review view, streaming build,
   `Continue?` elevation gate.
3. **repo-install**: `aurox -S repo-hello` — the pacman-parity fast path,
   straight to the disclosed pacman command; the fixture carries a ~5 MiB
   deterministic payload + paced `.install` scriptlet so pacman's figures
   and timing read real.
4. **upgrade**: bare shell `upgrade` — whole system, like `pacman -Syu`.
   Hermetic because `demos/seed-upgrade.sh` (shared with extended/36) both
   seeds the outdated installs and strips core/extra from pacman.conf, so
   "all pending updates" truthfully is the two fixtures. Mixed repo + AUR
   change set, real 📥 total, two elevation gates (`-Syu` lane, then `-U`).

Possible later: `-R` removal preflight; the three-way first-launch consent;
initial mirror clone (time-compressed cast) and incremental `-Sy` refresh —
see docs/TODO.md "Demos".

## Phases

1. **(done)** Recorder in pty-harness; `run.sh --record`; CI `pty-casts`
   artifact; TESTING.md docs.
2. **(done)** `send_human`/`dwell` pacing in pty-harness;
   `examples/demo_search_install.rs` (+ extended-tier test 33 so the flow
   can't rot); image bakes agg 1.9.0 (sha256-pinned) + JetBrains Mono +
   Noto Color Emoji; `demos/build.sh`; README hero GIF
   (`docs/demo/search-install.gif`, 833 KB / 20.6 s).
3. **(done)** Sidecar repo
   [aurox-ci-media](https://github.com/nikicat/aurox-ci-media): `main/`
   demos + self-hosted asciinema-player, a synced base-vs-PR `compare.html`,
   and a base-vs-PR `diff.html` (normalized transcript diff) on Pages
   (<https://nikicat.github.io/aurox-ci-media/>), pushed to by the Screencasts
   workflow (`.github/workflows/screencasts.yml`) which records the demo set
   on UI-path PRs, publishes `pr-<N>/` (GIFs + casts + `.txt` transcripts),
   refreshes `main/` on merges, and attaches a `screencasts` check run
   carrying the GIF gallery + player + side-by-side + text-diff links (no PR
   comment — the gallery would swamp the conversation; fork PRs skipped —
   needs the `CI_MEDIA_DEPLOY_KEY` secret). Change gate is the path filter +
   the human-judged side-by-side/diff (see "Change detection" above).
   Follow-up content only: remove/first-launch/clone/refresh demos
   (docs/TODO.md).

## Findings from the hero demo (the review loop paying out)

Watching the first recording surfaced real UX issues no PTY assertion had:

- **`-> total 📥 ?` on all-unknown batches** — fixed: `total_line` now drops
  a term with nothing measured (same rule the 🔨 term already had) and the
  whole line when neither figure exists. Second review round extended the
  same rule to the one-line `apply` summary (`· ? · ? build` was still on
  camera): unknown size/build terms are dropped there too, never faked as
  `0s`.
- **Uncolored pacman output** — environment, not product: Arch ships
  `pacman.conf` with `Color` commented out; the demo record step enables it
  for the demo container only (the test suite greps uncolored output).
- **Search rows wrap at 100 cols** — open follow-up: long descriptions wrap
  mid-word on the grid. Proposal: a two-line row mode in `ui/grid.rs`
  (pacman-style `repo/name version` + indented description line), which
  fixes narrow terminals product-wide. Touches the table-unification seams
  and the PTY tests that compact-match wrapped lines — its own change, not
  part of this plan's phases.

## Measurements (2026-07-17, aurox-test container)

Recorded a real 25 s session (sync refresh, `-Ss`, `-S` building two AUR
fixtures, `-Si`, `-R`, glyph line) via `asciinema record --headless
--window-size 100x30`, rendered with agg 1.9:

| artifact | size |
|---|---|
| cast (221 events) | 15 KB |
| GIF font-size 16 (979×694) | 1.4 MB |
| GIF font-size 20 | 1.9 MB |
| gifsicle `-O3 --lossy=80` repass | 1.1 MB (not worth it) |

Emoji (`📥🔨` via Noto Color Emoji auto-fallback), arrows, and full color
render correctly in agg. `SOURCE_DATE_EPOCH=946684800 makepkg` pins
`builddate = 946684800` but the console still prints the real date — hence
the transcript scrub list.
