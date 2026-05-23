# gitaur vs. other Arch package managers

How gitaur relates to `pacman`, `yay`, and `paru` — what's the same, what's
deliberately different, and a few questions still open for design discussion.

This isn't a feature-matrix marketing comparison; it's an engineering doc
that explains the *behavioral* differences a user will actually notice, plus
the reasoning behind them.

## Quick orientation

| | pacman | yay | paru | gitaur |
|---|---|---|---|---|
| Language | C | Go | Rust | Rust |
| Repo packages | itself | wraps pacman | wraps pacman | wraps pacman |
| AUR source | — | RPC | RPC | local mirror clone (`gix`) |
| AUR metadata | — | RPC JSON | RPC JSON | rkyv-archived index of every pkgbase |
| PKGBUILD review | — | diff vs prev install | diff vs prev install | diff vs ancestor commit by `.SRCINFO` version |
| Build orchestration | — | sequential | sequential | stratified by makedepends DAG |
| Sudo prompts per `-S` | per call | per AUR pkg | per AUR pkg | one batched `pacman -U` at end |
| Configuration | `pacman.conf` | `~/.config/yay/config.json` | `~/.config/paru/paru.conf` | `~/.config/gitaur/config.toml` |

Read on for the parts that aren't obvious from the matrix.

## What gitaur does the same as yay/paru

- **Drives pacman for repo work.** AUR helpers don't reinvent dependency
  solving for the sync DBs; gitaur shells out to `pacman -S --needed
  --noconfirm` for the repo half of a transaction, same as the others.
- **Makepkg for building.** AUR pkgs build via the user's own `makepkg`
  invocation with the user's config.
- **Foreign pkg classification.** Any pkg in localdb that isn't in any
  syncdb is "foreign" — could be an AUR install, an old extra/ pkg that's
  since been dropped, or a manually-built `pacman -U`. All three helpers
  walk localdb the same way for this.
- **PKGBUILD review before build.** All three pause for human eyeballs
  between fetch and build.

## Where gitaur diverges

### AUR data source: full mirror clone instead of RPC

yay and paru hit `aur.archlinux.org`'s RPC for both search and dep
resolution. gitaur instead clones the entire AUR mirror
(`github.com/archlinux/aur.git`) into `~/.local/state/gitaur/aur` and
builds a local rkyv index over every pkgbase's `.SRCINFO`.

Why:
- **No RPC dependency.** Index queries are filesystem reads; offline `-S`
  /  `-Qu` work as long as the cached index isn't too stale.
- **Full git history per pkgbase.** Lets the PKGBUILD review diff against
  the *commit that produced the user's installed version*, not just the
  current HEAD vs nothing. yay/paru can only diff against the last
  build's commit they remember (or fall back to the full file).
- **Cheap repeated queries.** A `-Qu` doesn't pay for an RPC round trip
  per foreign pkg; the rkyv index is mmap'd and dispatch is `HashMap`
  lookups.

Cost: initial `-Sy` is heavier (clones the mirror — minutes on first
run); ongoing `-Sy` is `git fetch` plus a re-index of changed pkgbases
(~hundreds of ms typical, scales with churn since the last fetch).

### Typed name identities (`PkgName` / `PkgBase` / `PkgTarget` / `VirtualName`)

paru's code passes names as `String` / `&str` throughout, disambiguating
"is this a pkgname or a pkgbase?" by variable name and `match` arm.
That's also what yay does. gitaur lifts the distinction into the type
system — see [`src/names.rs`](../src/names.rs) for the four newtypes and
the rationale.

What this catches at compile time:
- Passing an AUR pkgbase string to a function expecting a localdb pkgname
- HashMap collisions when an AUR pkgbase ships a pkgname matching an
  unrelated pkgbase (the `commit-mono-font` regression — see the
  `pkgname_collision_with_another_pkgbase_does_not_leak_into_plan` test)
- Mixing `provides=` virtuals with real pkgnames at API boundaries

This is the part of gitaur with the most invasive refactor relative to
prior art. The four-type split has no equivalent in yay or paru.

### Typed versions with `vercmp`-by-default

paru wraps `alpm::Version` at compare sites but stores raw `String` in
its own structs. gitaur defines its own `Version` / `Ver` pair (in
[`src/version.rs`](../src/version.rs)) and pipes them through `IndexEntry`,
`PkgUpgrade`, `PacmanIndex.installed`, `InstalledCounterpart`, etc. — so
`<` and `==` on a version field always invoke vercmp, never lexical
comparison.

Practical effect: `Version::from("1.10") > Version::from("1.9")` (vercmp),
matching pacman's own ordering. Lexical comparison would give the wrong
answer.

### Counterpart resolution with provenance and hints

This is the part that's most novel and deserves its own subsection.

When an AUR pkgbase upgrade lands, the review screen needs to answer
"what does the user have installed that this build will displace?" so it
can label the screen (`install` / `reinstall` / `upgrade`), pick a diff
base, and write a sensible fallback note when no diff is found.

yay/paru answer this implicitly by checking the pkgname match. gitaur
makes the answer explicit via `PacmanIndex::counterpart_with_hint`,
which returns an `InstalledCounterpart { pkgname, version, via }` tagged
with how it matched:

- `via = Pkgname` — entry's own pkgname matched
- `via = Replaces` — entry's `replaces=` named an installed pkg
- `via = Provides` — entry's `provides=` (pkgname-scoped or
  pkgbase-level) named an installed pkg

The provenance shapes the review header ("upgrade: foo-ng 1.0-1 → 2.0-1
[replaces old-foo]"), drives a different fallback-note phrasing per tier,
and surfaces in the noconfirm trace for container tests.

**Hint plumbing.** When multiple installed pkgs match the same pkgbase's
`provides=`, the user's typed name (from `-S foo` argv or the `-Syu`
picker row) is plumbed as a `Target::hint` through expand → resolve →
prepare, biasing the lookup to the pkgname the user actually meant. Two
`tracing::warn!` diagnostics fire when this kicks in:

- `multiple installed pkgs match this pkgbase's provides` — there's
  ambiguity to disambiguate
- `counterpart hint diverged from unhinted lookup` — the hint changed
  the picked pkgname vs. what the unhinted walk would return

See [`ARCHITECTURE.md#resolving-the-installed-counterpart-of-an-aur-entry`](ARCHITECTURE.md#resolving-the-installed-counterpart-of-an-aur-entry)
for the full table.

### Stratified build orchestration

yay and paru build AUR pkgs sequentially: build one, install one,
build next, install next. gitaur builds in *strata* — sets of pkgbases
whose build-time deps (`makedepends` + `checkdepends`) are all in earlier
strata.

Why: a single makepkg failure no longer aborts the whole batch.
gitaur marks the failed pkgbase, auto-skips anything downstream of it in
the makedeps graph (would have failed anyway with missing build deps),
and keeps building everything else. The final summary lists installed /
failed / skipped pkgbases so the user knows exactly what happened.

See `run_aur_pipeline` in [`src/build.rs`](../src/build.rs).

### Sudo deferred to one batched `pacman -U`

yay/paru prompt for sudo per AUR pkg's install step. gitaur builds
without sudo, then issues one `pacman -U` per stratum at the end, with
the sudo timestamp cache (typically 5–15 min) bridging strata. The
typical AUR session prompts for sudo once.

### Idempotent build cache via artifact filenames

If a pkgbase has already been built at exactly the AUR index's version,
its `.pkg.tar.{zst,xz}` files are still present in the worktree. gitaur
skips re-running makepkg and reuses the on-disk artifacts. No sidecar DB
— the artifact filename literally encodes `<pkgname>-<version>-<arch>`,
which is the cache key.

Re-running `-S foo` after declining the install just replays the install
step; re-running after a failed build with no PKGBUILD changes is a
no-op. VCS pkgs (`-git`/`-svn`/`-hg`/`-bzr`) never hit the cache because
their `pkgver()` overrides the static field at build time.

### Bounded history walk with explicit `BoundExceeded` vs `NotInLineage`

When the review screen wants to diff against the commit that produced
the installed version, gitaur walks the pkgbase's git history looking
for a commit whose `.SRCINFO::version()` matches `installed_ver`. The
walk is bounded by `Config::review_history_scan_max` (default 256).

If the walk doesn't find a match, the outcome is one of:
- `NotInLineage { walked }` — reached the branch root before the bound;
  bumping the config won't help. Fallback note explains likely causes
  ("EOL'd official repo, renamed pkgbase, sibling AUR pkg also providing
  this virtual").
- `BoundExceeded { bound }` — stopped at the bound; bumping might help.
  Fallback note points at the config knob.

yay/paru don't distinguish these cases (or don't surface them in the
review screen at all).

## Open design questions

These are decisions where gitaur's current behavior may not be the best
default; documented here so contributors can pick them up.

### Provides-based upgrade rows in `-Syu`

**Current gitaur behavior:** if `dotnet-runtime-7.0` is installed
(foreign) and AUR pkgbase `dotnet-core-7.0-bin` declares
`provides=dotnet-runtime-7.0`, gitaur's `-Syu` shows the row as an
upgrade candidate (typed `Target::hint` = the foreign pkgname).

**yay behavior:** shows `dotnet-runtime-7.0` as "Packages not in AUR"
and does *not* propose an upgrade. Conservative.

**Tradeoffs:**

- gitaur's behavior surfaces real migration paths (genuine pkg renames
  where `provides=` is the only signal) but conflates them with EOL
  replacements where the version schemes are incompatible (the
  motivating example: Microsoft's `sdk120` numbering vs the AUR
  maintainer's `sdk410` numbering — same `pkgver=7.0.20` prefix,
  unrelated versioning underneath, `vercmp` accidentally says "older").
- yay's behavior matches the "don't act surprisingly" mental model but
  hides discoverability — the user has no way to learn that
  `dotnet-core-7.0-bin` exists without going looking.

**Proposed middle ground (not implemented):**
1. Demote provides-only upgrades from the main `-Su` picker by default.
2. After the picker, print a separate info section:
   ```
   Foreign pkgs with AUR providers (not auto-proposed):
       dotnet-runtime-7.0  7.0.20.sdk120-2  →  dotnet-core-7.0-bin 7.0.20.sdk410-2  [provides]
   Run `gitaur -S dotnet-core-7.0-bin` to migrate.
   ```
3. Keep the existing `-S <name>` flow unchanged — explicit naming is
   already an opt-in to the migration.

This gets yay's safety-by-default *and* gitaur's discoverability —
neither has both.

A config knob (`syu_propose_provides_upgrades = false` default) would
let users opt back into the current behavior.

### Auto-pre-selection of AUR rows in the picker

`Config::aur_default_select` controls whether AUR upgrade rows are
pre-checked in the `-Syu` picker. Default is `false` (gitaur's choice
— "AUR is opt-in"); yay/paru's default is effectively `true` (every
upgrade pre-selected).

Open question: should the default flip? The current default is
deliberate but unfamiliar to yay/paru migrants. Worth surfacing in the
first-run experience either way.

### Should foreign pkgs without an AUR home appear in `-Qu`?

`-Qu` today reports only pkgs with newer versions available somewhere.
A foreign pkg that's not in any syncdb AND not in AUR is invisible to
`-Qu` — silently orphaned.

yay's "Packages not in AUR" message in `-Syu` is one way to surface
this. gitaur could add a `-Qf` (foreign-not-in-AUR list) or fold it
into `-Qu`'s output. Open.

### Index format upgrades and the rkyv FORMAT_VERSION

When a typed-wrapper refactor changes a struct's Rust type identity (even
when the on-disk bytes happen to match), rkyv archives become
incompatible. Phase A bumped FORMAT_VERSION 2→3. Phase B didn't bump
because the version-storage shape stayed `String` at the SRCINFO-field
level — `IndexEntry.pkgver`/`pkgrel`/`epoch` are still raw parser output;
only the combined `Version` lives at higher layers.

Future Phase C (typed `Pkgver`/`Pkgrel`/`Epoch` fragments) would force a
bump. Not on the immediate roadmap because those fragments aren't
vercmp-comparable in isolation — wrapping them would invite the
wrong-API trap (`pkgver < pkgver` is meaningless).

## Pointers for further reading

- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — the design doc for gitaur's
  internals, including the counterpart resolution table, the
  [resolution case matrix](ARCHITECTURE.md#resolution-case-matrix) (every
  installed-state × pkgbase-declares shape with its test pointer), and
  the build-pipeline walkthrough.
- [`src/names.rs`](../src/names.rs) and [`src/version.rs`](../src/version.rs)
  — the typed-identity refactor commentary.
- [`tests/container/smoke/`](../tests/container/smoke/) — end-to-end
  regression tests for the trickier behaviors documented above (provides
  rename, multi-match counterpart, `-Syu` hint plumbing).
