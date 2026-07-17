# Plan: shell UX — WYSIWYG addressing + ambient state

Status: **planned**. Amends [shell-ui.md](shell-ui.md): supersedes its post-5c
backlog item 2 ("`add`/`drop` should print the whole cart") and replaces the
`View` mechanism from phase 5. The cart, verbs, selectors, undo/redo, and
consent points are untouched — this re-plumbs *addressing* and *visibility*.

## The incident

```
aurox> search 3dslicer
  …6 numbered rows, best (row 1 = 3dslicer) nearest the prompt…
aurox> add 1
staged 3dslicer (aur)
transaction — 2 to install, 0 to remove
1  aur  review    3dslicer      5.12.2-1
2  aur  approved  3dslicer-bin  5.10.0-1
…
aurox> drop 2
3dslicer-git wasn't staged
```

The user read `2` off the transaction table the shell had just printed;
the shell resolved it against the *search list* (`View::Search` — `add`
deliberately doesn't flip the view so `add`-runs keep working), where row 2 is
`3dslicer-git`. Here the collision was a confusing no-op; had that search row
been staged, `drop`/`approve` would have silently hit the wrong package —
`approve N` clearing the review gate on a PKGBUILD the user never read.

## Root causes

1. **Numbers are indices into hidden live state, not names for what was
   printed.** The `View` mode is invisible; what's printed (env side) and
   what's addressable (`State.view`) are maintained at two sites and drift —
   the same disease the one-schema-one-site rule exists to kill. A second
   variant: cart lists rebuild live, so a `drop` renumbers remaining rows
   *under* a user working through a printed table.
2. **State visibility by shouting.** The post-mutation full-table reprint
   exists because there's no other way to keep state on screen — and it's what
   puts dead numbers on screen in the first place.
3. **Hints stop one step short** ("1 package(s) need review — run
   `review <sel>`" — the shell knows the name; tables full of `?` cells).

## Design

### A. WYSIWYG addressing (the referent)

> A bare number is a *name for a row of the last numbered table printed*.
> Tables printed without row numbers are not addressable. There is no other
> rule to remember.

- `State` replaces `view: View` + `search_list` with one field: the
  **referent** — a snapshot of the rows of the last numbered table, tagged
  with its kind (search results / transaction) for error wording.
- The referent is set **only** where a numbered table is printed — `search`
  (non-empty results) and `show` (non-empty cart; `upgrade`, `undo`, `redo`,
  and the apply-failure path print through `show`). Structurally one seam:
  render-and-capture happen together, so screen and referent cannot drift.
- **Snapshot semantics**: numbers resolve against the snapshot, i.e. to *the
  package shown at that number* — even if the cart re-sorted or shrank
  meanwhile. A row whose package has since left the cart is a clean miss
  ("row 2 (3dslicer-bin) is no longer staged"), never a silent wrong hit.
  This is why `show; drop 2; drop 4` works with no reprint between drops.
- A fruitless `search` (and `show` on an empty cart) prints no numbered rows,
  so it leaves the referent alone — the table still visible above stays
  addressable. (Reverses the phase-2 "replace even when empty" rule, which
  served the old philosophy.)
- Names/globs/repo-tokens keep their verb-scoping (cart verbs match staged
  rows, list verbs the universe) — numbers were the only shared referent,
  hence the only trap.

### B. Quiet mutations, ambient state

- `add` / `drop` / `keep` / `remove` stop reprinting the transaction. They
  print their per-target acks plus **one summary line**:
  `cart: 2 to install — 1 needs review (`review 3dslicer`)`
  `cart: 2 to install, 1 to remove — all approved; `apply` when ready`
  `approve`/`review` print the same line when they changed something — the
  "all approved" moment is worth surfacing. Counts/approval wording is shared
  with `show`'s header/footer (one site).
- The **prompt carries standing state**:
  `aurox> ` (empty cart) · `aurox [3 staged]> ` · `aurox [3 staged, 1 to review]> `
  Classic fix for hidden modes: make state ambient instead of loud.
- The full table lives behind `show` (new alias: `cart`) — which is also the
  deliberate "give me fresh numbers" gesture.
- `upgrade`/`undo`/`redo` keep printing the full table: they change the cart
  wholesale in ways the user didn't enumerate, so presenting the result *is*
  their job (and it re-arms the referent).

### C. Language polish

- **Acks name the resolution** when a number was used: `dropped 3dslicer-bin
  (row 2)` — selector resolution carries row provenance.
- **Misses teach the model**: `row 2 of the search results = 3dslicer-git —
  not staged; `show` numbers the cart` instead of "3dslicer-git wasn't
  staged".
- **Fill in hints**: singular pending review names the package and the exact
  command; real plurals everywhere ("1 upgrade staged", not "1 upgrade(s)").
- **Unknown first word**: fuzzy-match verbs+aliases (edit distance ≤ 2) —
  "did you mean `approve`?" — else suggest `search <word>`. Requires lifting
  the alias table out of `parse`'s match into data (one site, reused by the
  suggester).
- **`?` cells**: unknown values render `—`; an all-unknown column is dropped;
  the total line says what's unknown once instead of `📥 ?`.

## Rejected alternatives

- **Verb-scoped numbers** (`drop N` = cart, `add N` = search): the same digit
  meaning different rows for different verbs on one screen is a worse trap.
- **Dual address spaces** (`c1`/letters for cart rows): unambiguous but a
  second syntax; snapshot semantics gets the safety with plain numbers.
- **Full TUI / pinned status region**: solves visibility but abandons the
  line-oriented scrollback model (locked decision in shell-ui.md); the status
  prompt gets most of it. Revisitable later as `--tui`.

## Phasing (one PR, three commits)

1. **Referent + quiet mutations** — the semantic core, indivisible: numbered
   reprints that hijacked the referent and add-run ergonomics only reconcile
   with both halves in place. Kills `View`; mutations print the one-line
   status (header counts + approval standing, shared wording with `show`) in
   place of the table; updates help text and the container/PTY needles that
   expected post-mutation tables.
2. **Status prompt** — additive visibility layer (`State::prompt()` carrying
   the cart counts into the readline prompt).
3. **Polish** — row provenance in acks/misses, plurals, unknown-verb
   suggestions, `cart` alias, `?`-cell handling.

## Testing

- Unit (dispatch + selector): the incident as a regression test; snapshot
  stability across cart mutations; referent untouched by quiet verbs /
  fruitless search; prompt/summary wording.
- PTY e2e + container smoke: update needles (no post-`add` table; new prompt
  shape — `aurox>` prefix still matches; `pty_harness::has()` whitespace
  rules per the existing race notes). Apply lanes are only lightly touched
  (no view flip), but run the shell container tests before merge.
