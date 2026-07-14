# aurox — agent notes

`aurox` is a Rust AUR helper with pacman-parity UX (the name: AUR + the
gitoxide/Rust-oxide ox; its predecessor name `gaur` — another wild ox — was
taken on the AUR).

## Design conventions

Hard-won rules from review; hold new code to these.

- **One schema, one site.** Never retype a schema fact (a serde key name, a
  default value) at a second site — derive it (`optfield` generates
  `ConfigFile` from the one `Config` definition) or arrange for drift to be
  a compile error (exhaustive struct literals). A string literal that must
  match a field name is a latent desync.
- **Model state, don't reconstruct it.** If code needs to know "did the user
  set this?", that is state — carry it in a type (`Option` fields on the
  on-disk schema), never recover it by diffing serializations or splicing
  documents.
- **Persist sparsely.** A config/state write must not materialize current
  defaults into user files; absent keys keep following upgraded defaults.
  Broader rule: persist nothing derivable from artifacts or pacman DBs.
- **Bind data to its provenance.** A value loaded from a path it may be
  written back to travels *with* that path (`ConfigHandle` = file + path +
  resolved view), and a change is ONE operation updating disk and the
  in-memory view together. No ambient `paths::…()` lookups at write sites;
  no hand-mirrored `cfg.x = …` after a file write.
- **Absent provider = empty provider.** An optional data source loads as
  *empty* at one seam (`AurIndexData::load`) so downstream code takes one
  uniform path; a state enum probed once (`AurState`) feeds user-facing
  wording only, never data flow. Don't scatter availability checks.
- **Name the implicit entity.** A parameter caravan (≥4 args) or a fn family
  threading the same tuple is an unnamed type — introduce it (`InstallCtx`,
  `PipelineRun`, `ChangeSet`, `ReviewRequest`); prefer methods on the owning
  type over free functions.
- **Consent at a decision point.** Expensive or irreversible actions ask at
  a moment the user can think (the shell's launch question), not at whatever
  call site happens to trigger them. After an informed explicit command,
  don't double-prompt; on implicit triggers, refuse rather than surprise.
  Complicated decision logic gets a table in the module doc
  (`mirror/consent.rs`) and pure, parameter-injected decision fns.

## Testing

See **[docs/TESTING.md](docs/TESTING.md)** for the full picture. Two layers:
`cargo test` (unit + `tests/*.rs`, hermetic) and the container suite
(`tests/container/run.sh`, end-to-end in a throwaway Arch container).

**The one gotcha:** the container image bakes fixtures (`fixtures/*/`) at
*build* time, so after changing a fixture, the `Dockerfile`, or
`setup-fixtures.sh` you **must** pass `--rebuild` (e.g.
`tests/container/run.sh --rebuild smoke/59_*.sh`) — otherwise you're testing a
stale image, not a code bug. Source-only changes never need it. The why, and
the parallelism-flake caveat, are in docs/TESTING.md.
