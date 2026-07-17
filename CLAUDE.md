# aurox — agent notes

`aurox` is a Rust AUR helper with pacman-parity UX (the name: AUR + the
gitoxide/Rust-oxide ox; its predecessor name `gaur` — another wild ox — was
taken on the AUR).

## Commits

**Never `git commit` without explicit approval.** In every session, no
exceptions (background jobs, "trivial" fixes, follow-up commits included):
finish the chunk, run the tests, summarize the diff, then ask and **wait**
for the user's explicit go-ahead before committing.

## Design conventions

Hard-won rules from review; hold new code to these.

- **One schema, one site.** Never retype a schema fact (a serde key name, a
  default value) at a second site — derive it (`optfield` generates
  `ConfigFile` from the one `Config` definition) or arrange for drift to be
  a compile error (exhaustive struct literals). A string literal that must
  match a field name is a latent desync.
- **Don't hand-maintain derivable counts.** A const data table is `&[T]`,
  not `[T; N]`: a `const`'s type can't infer `N` on stable, so the literal
  length is a number every row add/remove must edit — noise even though the
  compiler checks it. Tests assert against `TABLE.len()`, never a re-typed
  count.
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
- **Clone freely above the data plane — but at one named place.** In the
  shell/UI/cart layers object counts are tiny: a `.clone()` there is fine and
  often *is* the semantics (undo snapshots, the referent snapshot); never
  contort an API with lifetimes to save one. Repeated per-field clone
  caravans move into a named seam — a `From` impl (`ListItem`/`RepoRow`) or
  a single owner (`edit_cart` for undo). In per-entry loops over the
  index/refs/alpm (100k+ items), borrow — measured history: the 155k-ref
  fetch path. `redundant_clone` is enforced, so a surviving clone is
  structural; when reviewing one, the question is "which copy is the truth
  afterwards?", not "how many bytes?" (that question found the apply
  reviewed-set loss).

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
