# aurox — agent notes

`aurox` is a Rust AUR helper with pacman-parity UX (the name: AUR + the
gitoxide/Rust-oxide ox; its predecessor name `gaur` — another wild ox — was
taken on the AUR).

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
