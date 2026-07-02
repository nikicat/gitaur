# TODO

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
