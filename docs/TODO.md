# TODO

## Shell

- Ctrl-C during *any* long-running shell operation should bail out to the
  prompt, never stop aurox. Builds already behave (extended/31 pins the
  apply-build ^C, extended/02 the `-S` forward-to-makepkg half); the
  repo/AUR *refresh* does not — ^C mid-fetch currently takes the whole
  shell down.
- `upgrade` runs the AUR refresh unconditionally whenever the AUR is
  enabled in config: there is no way to upgrade just pacman packages
  without sitting through the AUR fetch first — bad UX on a slow-mirror
  day. The repo half should be reachable without (or before) the AUR half;
  with the item above, ^C during the refresh could also degrade that
  upgrade to repo-only instead of aborting it.
- search results should be colored — the shell's numbered list renders as a
  dim monochrome table (`src/ui/search_table.rs`) while `-Ss` styles
  repo/name/version. Whatever palette lands, the installed flag must stay
  clearly visible (today it's row emphasis plus the `old → new` version
  cell, which color alone could drown out).
- two-line search/upgrade table rows, pacman-style (`repo/name version` line
  + indented description line) via a `ui/grid.rs` row mode — long
  descriptions currently wrap mid-word on narrow terminals (surfaced by the
  README screencasts; see the finding in docs/plans/screencasts.md).
  Touches the table-unification seams and the PTY tests that compact-match
  wrapped lines.
- noticeable delay on exit: quitting takes a visible beat before the
  terminal prompt returns. Not reproducible at fixture scale — the hero
  demo cast measures quit → bash prompt at ~10 ms — so profile against a
  real-sized state (~2 GiB mirror, 155k-package index): dropping the
  zero-copy index mmaps, gix teardown, and the tracing file-log flush are
  the first suspects.

## Demos (docs/plans/screencasts.md)

- initial AUR mirror clone, sped up: the one-time ~2 GiB clone with its
  progress UI, time-compressed to ~15 s. The mock mirror clones instantly
  (nothing to show) and a live recording is non-hermetic — the pragmatic
  path is a hand-recorded real clone whose cast timestamps are rescaled
  (asciicast times are trivially editable), with the `.cast` checked in as
  the source so the GIF still renders reproducibly.
- incremental refresh: `-Sy` after a branch moves on the mirror — reuse
  extended/18's hermetic bump mechanics (clone the mock-AUR branch, commit
  a pkgver bump, fetch it back) to show "no ref updates" vs
  "1 ref(s) updated" + the index catching the new version.

## AUR

- show time since last commit for aur packages in upgrade/install tables (like in yay)
- account for already downloaded sources when printing download sizes in tables
- save review approvals for concrete versions persistently

<!-- Done:
- remove ~ before times/sizes: the approximate prefix is gone everywhere
  (per-cell + totals + search list); an estimate now reads as the bare figure.
  A *summed* total that under-counts because a row's figure is unknown is a
  lower bound, rendered `>XXhYYm` / `>N MiB` instead. (src/ui/cost.rs +
  src/ui/change_set.rs)
- never-built build-time no longer renders `~0s build`: an all-unknown build
  total is `? build`; TimeEst/SizeEst totals collapse to their own figure kind.
-->

## Related design note

The build-time figure is a real `TimeEst` (`Estimate(Duration)` / `Unknown` /
`None`) and its per-batch *total* is a `TimeTotal` (`Measured{total,bound}` /
`Unknown` / `None`); size mirrors it (`SizeEst` cell, `SizeTotal` total). The
`bound: Bound::{Exact,Lower}` on a total is what prints the `>` lower-bound
marker when an unknown row drags the sum below the true value.
