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

## Package size not shown for pacman packages in the update table

In the update/upgrade table screen, the package size column is blank for plain
pacman (repo) packages. Size should be populated from the sync DB entry for repo
packages, the same way it is for AUR packages.
