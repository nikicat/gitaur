# Bugs

## systemd-selinux fails with self-cycle in dependency resolution

`systemd-selinux` can't be installed:

```
error: resolve: cycle: systemd-selinux → systemd-selinux
```

The resolver reports a self-referential cycle (`systemd-selinux → systemd-selinux`).
A package depending on / providing itself (or otherwise resolving to itself) must
not be treated as a dependency cycle — self-edges should be dropped before cycle
detection.

<!-- Resolved/obsolete: "Package size not shown for pacman packages in the
update table." That table was the interactive `-Syu` picker, now removed; the
shell's change-set preview already shows repo `download_size` from the syncdb
(see `src/ui/change_set.rs`, UPDATE_LOOP phase 2). -->

_(One open bug remains above. Feature roadmaps live in the per-feature plan
docs: shell phases 5–6 in `shell-ui.md`, GPG key import in
`gpg-key-auto-import.md`, the `-Syu` discoverability + `-Qf`/`-G` ideas in
`../COMPARISON.md`'s "Open design questions", and the test backlog in
`../../tests/container/extended/.scope`.)_
