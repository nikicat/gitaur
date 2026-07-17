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

- **Check run always** (on PRs touching `src/ui/`, `src/cli/shell/`,
  `demos/`, fixtures): the Checks API `output` carries markdown (≤64 KB)
  plus an `images[]` gallery — GIF URLs from the sidecar repo. Checks are
  SHA-bound, so every push keeps its own screencasts (a browsable history a
  sticky comment would destroy). Default `GITHUB_TOKEN` with
  `checks: write` suffices.
- **Sticky comment only when something changed** — the loud, notifying
  channel is reserved for real visual changes (hidden HTML marker to find
  and update one comment in place).
- **Side-by-side**: inline via `ffmpeg -filter_complex hstack` of base|PR
  GIFs (pad the shorter with its last frame); later a small Pages page with
  two asciinema-player instances slaved to one scrub bar (seek API).

## Change detection: keyframe transcripts

"Did the UI output change vs base?" — byte-diffing recordings is
meaningless, so:

- Phase 2/3: the demo driver dumps the vt100 `screen()` at each quiescent
  point (command finished, prompt back) into a **normalized text transcript**
  committed to the repo (insta-style snapshots). Normalization scrubs the
  only known wall-clock leaks: makepkg's `Making package:`/`Finished
  making: … (<date>)` suffixes (verified real) and seconds-level duration
  strings. `-Si` dates are already deterministic via fixture `commit-date`.
- "Changed relative to base" then falls out of the PR's own text diff —
  reviewable in the files tab — and CI posts GIFs only when a transcript
  changed. CI enforces transcript freshness (regen must be a no-op).
- Until then (phase 1/2), the path filter above is the trigger; the cost is
  an occasional GIF for a visually-neutral refactor.

## Demo set (each 15–30 s, 100 cols)

1. **Hero** (top of README): bare `aurox` shell — banner, search, stage by
   number, `apply` with change-set table and sudo gate.
2. **Upgrade**: `upgrade` → change-set with 📥/🔨 totals → apply.
3. **Remove**: `-R` preflight refusal + `-Rc`.
4. Maybe: the three-way first-launch sync consent (instant against the mock
   mirror — honest enough for a demo).

## Phases

1. **(done)** Recorder in pty-harness; `run.sh --record`; CI `pty-casts`
   artifact; TESTING.md docs.
2. First demo driver (search+install) + pacing helpers; agg render script;
   image bakes agg+fonts; README hero GIF.
3. Remaining demos; sidecar repo + Pages player; check-run/sticky-comment
   workflow; keyframe transcripts; side-by-side.

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
