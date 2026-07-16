# Bugs

_No open bugs._

<!-- Resolved: systemd-selinux self-cycle in dependency resolution.
`error: resolve: cycle: systemd-selinux → systemd-selinux` — a pkgbase that
named itself in `depends` (a package providing/depending on its own name) built
a self-edge in the full-graph cycle check (`topo::sort` over `all_edges`). Fixed
in `src/resolver.rs::resolve` by dropping deps that `refers_to` the owning
pkgbase before the cycle check (mirroring the self-edge filter
`resolve_make_edges` already applied for the strata pass). `topo.rs` keeps its
invariant — a genuine self-loop in raw input is still a cycle; the resolver just
never constructs one. Regression tests: `resolver::tests::aur_self_dependency_is_not_a_cycle`
+ `aur_self_makedepend_is_not_a_cycle`. -->

<!-- Resolved/obsolete: "Package size not shown for pacman packages in the
update table." That table was the interactive `-Syu` picker, now removed; the
shell's change-set preview already shows repo `download_size` from the syncdb
(see `src/ui/change_set.rs`, UPDATE_LOOP phase 2). -->

_Feature roadmaps live in the per-feature plan docs: shell phases 5–6 in
`shell-ui.md`, GPG key import in `gpg-key-auto-import.md`, and the `-Syu`
discoverability + `-Qf`/`-G` ideas in `../COMPARISON.md`'s "Open design
questions". The container-test backlog (`../../tests/container/extended/.scope`)
is fully landed as of 2026-07-16; new planned tests go back in that file._
