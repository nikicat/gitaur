# Profiling aurox refreshes

A plain `aurox -Sy` against the AUR mirror takes ~10 s on a warm cache;
most of that is gix-internal time on a ref store with 154k branches.
`scripts/profile-refresh.sh` captures a samply CPU profile and prints a
flat self/total time table so you can see which gix functions dominate.

## One-time setup

```sh
cargo install samply
sudo sh -c 'echo 1    > /proc/sys/kernel/perf_event_paranoid
             echo 2048 > /proc/sys/kernel/perf_event_mlock_kb'
```

Both sysctls reset on reboot. `perf_event_paranoid<=1` lets non-root
users open perf events; `perf_event_mlock_kb>=2048` raises the per-CPU
ring-buffer allowance samply needs.

## Run

```sh
scripts/profile-refresh.sh                       # profiles `aurox -Sy`
scripts/profile-refresh.sh -- -S some-package    # any args after --
scripts/profile-refresh.sh -o /tmp/p.json.gz     # custom output path
```

Outputs `profile.json.gz` + `profile.json.syms.json`. Open interactively
with `samply load profile.json.gz`.

## What to look for

Two silent gaps inside `gix::Prepare::receive()` dominate:

1. **have-set build** (between `prepare_fetch returned` and the first
   `negotiate (round N)` log line) — gix walks local refs to populate
   the wire-protocol "have" list. Surfaces in samply under
   `Negotiate::mark_complete` and its descendants.
2. **post-pack ref update** (between the last `read pack done` and
   `receive returned`) — gix rewrites refs by calling
   `repo.try_find_reference()` once per advertised mapping. Surfaces
   under `gix::remote::connection::fetch::refs::update`.

Both phases bottom out in
`gix_ref::store_impl::packed::find::binary_search_by`, which calls
`packed::decode::reference` ~17× per query, and each decode runs
`gix_validate::tag::name_inner`. On a 154k-ref store the validation
re-cost is the single largest CPU sink.

## Testing a gix patch locally

The repo at `../gitoxide` can be plugged in via `[patch.crates-io]`
in `Cargo.toml`:

```toml
[patch.crates-io]
gix     = { path = "../gitoxide/gix" }
gix-ref = { path = "../gitoxide/gix-ref" }
```

Add only the crates you're actively patching — cargo will rebuild the
whole gix graph against the path versions.

## What's already been optimized

The two phases above have been heavily reworked in the gix fork. See
[`FETCH_OPTIMIZATION.md`](FETCH_OPTIMIZATION.md) for the full record —
every change with before/after numbers, the one known dead end, and the
remaining candidates.
