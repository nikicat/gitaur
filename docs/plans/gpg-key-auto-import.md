# Plan: GPG key auto-import for `gaur` AUR builds

Status: proposed (not yet implemented)

## Goal

Before building an AUR pkgbase whose `.SRCINFO` declares `validpgpkeys`,
detect which of those keys are absent from the user's GPG keyring and offer
to `gpg --recv-keys` them — turning makepkg's hard "unknown public key"
failure into a one-time prompt, matching yay/paru but keeping gitaur's
explicit/auditable stance.

Today gitaur runs `makepkg -d --noconfirm --needed` (`src/config/defaults.rs:20`)
with **no** `--skippgpcheck`, so a missing `validpgpkeys` key fails the build.
yay/paru add a helper-side key-import step layered on top of makepkg; gitaur
currently does not. This feature adds it.

## Locked design decisions

- **Default policy `ask`** — prompt with fingerprints before importing;
  `--noconfirm` auto-accepts (yay parity). Knob values: `never` | `ask` |
  `always`.
- **Source keys from `<worktree>/.SRCINFO`**, parsed with the existing
  `srcinfo::parse` (`src/index/srcinfo.rs:34`). No `IndexEntry` field, **no
  `FORMAT_VERSION` bump** (stays 6 — `src/index/schema.rs:171`), no forced
  re-index. Keys are only needed at build time, exactly when the worktree is
  checked out.
- **Batch once, up front** — collect across all strata between Phase 1 and
  Phase 2 of `run_aur_pipeline`, so the single prompt lands before any build
  (and before sudo), consistent with `feedback_defer_consolidate_sudo`.

## Custom types (instead of strings)

The existing config knobs (`color`, `review_default`, `privilege_escalator`)
are all bare `String` matched at use sites. This feature introduces real
types, honoring the typed-identifiers preference and setting a pattern those
older knobs could later follow.

### `GpgImportPolicy` enum (replaces `gpg_auto_import: String`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GpgImportPolicy {
    Never,
    #[default]
    Ask,
    Always,
}
```

Stored directly in `Config`; serde parses the TOML string at the deserialize
boundary (the one place a raw string is legitimate). The pipeline matches on
the enum — exhaustive arms, no `"ask"` literals. Strictly better than the
neighboring `color: String` / `review_default: String`.

### `PgpFingerprint` newtype (replaces `Vec<String>` of keys)

Transparent-over-`String` newtype mirroring `PkgName`/`PkgBase` in
`src/names.rs` — narrow surface (`From<&str>`, `as_str()`, `Borrow<str>`,
`Display` for rendering only; **no** `Deref`/`AsRef<str>`). A key fingerprint
can never be silently passed where a `PkgName`/pkgbase string is expected.
`validpgpkeys` are always full 40-hex fingerprints per makepkg's rule, so one
type covers it. Domain method `is_well_formed()` (hex + length) for an early
reject before shelling to gpg.

Lives in `src/gpg.rs` (build-time domain), not `src/names.rs` (scoped to
name-shaped strings) — keeps each module's invariant story clean.

### `KeyRequest` aggregate (replaces parallel vecs / tuples)

```rust
struct KeyRequest {
    fpr: PgpFingerprint,
    wanted_by: Vec<PkgBase>,   // which pkgbase(s) declared it — drives prompt text
}
```

`missing_keys` returns `Vec<KeyRequest>`; the prompt renders
"`ABCD… (wanted by foo, bar)`" without threading two collections.

### `Keyserver` newtype (replaces `gpg_keyserver: Option<String>`)

`Option<Keyserver>` — thin newtype, `None` ⇒ defer to the user's `gpg.conf`.
Borderline (just passed to `--keyserver`) but kept typed for consistency.

### Deliberately left as `String`

- **`gpg_path: String`** — matches the existing `makepkg_path: String`
  precedent ("path or name resolved via `PATH`"); `Command::new` takes it
  directly. Diverging to `PathBuf` would be inconsistent with its sibling.
- **`Error::Gpg(String)`** — error *messages* are human-readable rendering,
  not domain ids; a string payload is correct.

## Implementation

### 1. New module `src/gpg.rs`

Thin wrapper over the user's `gpg` (uses their real keyring / `GNUPGHOME`),
following the `std::process::Command` pattern in `src/pacman/invoke.rs:93`:

- `fn missing_keys(fprs: &[PgpFingerprint], cfg: &Config) -> Vec<KeyRequest>`
  — for each fingerprint run `gpg --list-keys --with-colons <fpr>` (output
  suppressed); collect the ones that exit non-zero.
- `fn recv_keys(keys: &[PgpFingerprint], cfg: &Config) -> Result<()>` —
  `gpg [--keyserver <ks>] --recv-keys <k…>`; non-success ⇒ `Error::Gpg`.
- Uses `cfg.gpg_path`, optional `cfg.gpg_keyserver`. `tracing` at entry +
  loop-summary per `feedback_trace_critical_points`.
- Inline `#[cfg(test)]` unit tests over `missing_keys` against a temp
  `GNUPGHOME` (per `feedback_tests_with_modules`).

### 2. Wire into the build pipeline — `src/build.rs`

At the seam **between Phase 1 and Phase 2** of `run_aur_pipeline` (after the
`prep_strata` loop ends, ~`src/build.rs:392`, before `RunReport::default()`):

- Skip entirely if `cfg.gpg_auto_import == GpgImportPolicy::Never`.
- Iterate `prep_strata`, keep only preps with `disposition ==
  Disposition::Build` (cached/skipped never run makepkg).
- For each, read `prep.wt.path.join(".SRCINFO")`, `srcinfo::parse` it, pull
  `validpgpkeys` into `Vec<PgpFingerprint>`; dedup into an ordered set,
  tracking `wanted_by`.
- `gpg::missing_keys(...)`; if empty, done.
- Gate on policy: `Always` ⇒ import; `Ask` ⇒
  `ui::prompts::confirm("Import N missing PGP keys?", opts.noconfirm)`
  (`src/ui/prompts.rs:13`) after listing the fingerprints + who wants them.
- On decline or import failure: `ui::warn` and **continue** — let makepkg
  fail loudly for that pkgbase; stratified orchestration already marks it
  failed and skips its dependents. No pre-marking needed.

Extract `import_missing_pgp_keys(cfg, &prep_strata) -> Result<()>` rather than
inlining, to keep `run_aur_pipeline` readable.

### 3. Config — `src/config.rs` + `src/config/defaults.rs`

```toml
gpg_auto_import = "ask"   # never | ask | always  -> GpgImportPolicy
gpg_path        = "gpg"
gpg_keyserver   =          # Option<Keyserver>, None ⇒ use gpg.conf default
```

Defaults: `GpgImportPolicy::Ask`, `"gpg"`, `None`.

### 4. Error — `src/error.rs`

Add `#[error("gpg: {0}")] Gpg(String)` variant.

### 5. Docs

- **`docs/COMPARISON.md`**: update the "Missing PGP keys" row — gitaur moves
  from "left to user's keyring (build fails loudly)" to "prompts to import
  (`ask` default; `never`/`always` configurable)". Add a short subsection
  under *Where gitaur diverges* noting yay/paru import as a helper-side step
  layered on makepkg, while gitaur batches the prompt once up front (before
  sudo) rather than per-pkg.
- **`docs/ARCHITECTURE.md`**: brief note on the build-pipeline insertion
  point + the worktree-`.SRCINFO`-not-index sourcing rationale.

### 6. Tests

- **Unit** (`src/gpg.rs`): `missing_keys` against a temp `GNUPGHOME` with one
  imported key and one absent fingerprint.
- **Unit** (`.SRCINFO` extraction): a fixture with `validpgpkeys` ⇒ assert
  parsed into `Vec<PgpFingerprint>`.
- **Container smoke** (`tests/container/smoke/`): new `NN_gpg_auto_import.sh`
  — a fixture pkgbase with `validpgpkeys`, isolated `GNUPGHOME`; assert (a)
  `--noconfirm` imports and build proceeds, (b) `gpg_auto_import=never`
  leaves the key absent and makepkg fails loudly. Mirror
  `tests/container/smoke/03_install_aur_pkg.sh`.

## Open sub-decision (minor)

Whether `validpgpkeys` is captured by `srcinfo::parse` into `IndexEntry` as a
transient (not serialized) field, or read by a small dedicated parse pass at
build time. Pick whichever keeps `IndexEntry` clean and avoids touching the
rkyv-serialized shape — likely a tiny standalone "extract validpgpkeys"
reader so the index struct is untouched. Confirm during implementation.

## Out of scope

- Importing keys for pkgs not being built; `-Qu`/preview showing signing
  requirements (would need the index field + format bump — explicitly
  rejected here).
- Key *trust* management / signing — only fetch into the keyring, same as
  yay.

## Code anchors

| File | Anchor | Purpose |
|------|--------|---------|
| `src/index/srcinfo.rs` | `parse()` @34 | reuse to extract `validpgpkeys` from worktree `.SRCINFO` |
| `src/index/schema.rs` | `FORMAT_VERSION` @171 | stays 6 — no bump |
| `src/build.rs` | `run_aur_pipeline` @351, Phase 1/2 seam @392 | insertion point |
| `src/build/makepkg.rs` | `run()` @46 | builds run after key import |
| `src/config/defaults.rs` | @20 | `makepkg_args` shows no `--skippgpcheck` |
| `src/ui/prompts.rs` | `confirm()` @13 | import prompt |
| `src/pacman/invoke.rs` | @93 | `Command` spawn pattern to mirror |
| `src/error.rs` | Error enum | add `Gpg(String)` |
| `src/names.rs` | newtype pattern | mirror for `PgpFingerprint` |
