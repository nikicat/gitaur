//! Black-box regression check for `full_build` using kernel resource
//! accounting. The per-iter `gix::open` bug manifests as per-branch
//! `mmap`/`openat` syscalls + page faults touching the pack `.idx`;
//! those land in `ru_minflt` (minor page faults from each fresh mmap)
//! and `ru_stime` (kernel time in mmap/VFS), neither of which our
//! `WORKER_REPO_OPENS` counter captures structurally.
//!
//! The fixture is sized + shaped to resemble the real AUR mirror in
//! miniature: a single repacked packfile + `packed-refs` with N entries,
//! built via `git fast-import` for speed (~100k obj/s, single
//! subprocess).
//!
//! Uses `RUSAGE_SELF` (process-wide) because rayon spawns worker
//! threads — `RUSAGE_THREAD` on the calling thread would miss every
//! `gix::open` happening inside a worker. Living in its own
//! integration-test binary keeps other concurrent tests out of the
//! snapshot.
//!
//! Linux-only: `ru_minflt` semantics are POSIX but the absolute counts
//! depend on the Linux page allocator. We don't assert on `ru_stime` or
//! context-switch counts — those drift too much on slow / containerized
//! CI runners (different filesystems, kernel versions, core counts).
//! `minflt` is structural: it scales with the number of fresh mmaps,
//! which is exactly what the bug inflates.

#![cfg(target_os = "linux")]

use aurox::config::defaults::default_config;
use aurox::index::build::full_build;
use aurox::mirror::MirrorRepo;
use aurox::testing::git;
use nix::sys::resource::{UsageWho, getrusage};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const N_BRANCHES: usize = 5_000;

/// Threshold sits ~7× above the post-fix observed value (~2.9k on a
/// 16-core Arch box) and ~2× below the bug's (~38k). Wide enough for
/// CI drift from allocator state, debug vs release builds, page-size
/// differences (e.g. 16 KB on Apple Silicon would lower fault counts
/// proportionally), and other `RUSAGE_SELF` noise.
const MINFLT_THRESHOLD: i64 = 20_000;

/// Stream `git fast-import` to build N branches, one commit each, each
/// commit's tree holding a unique `.SRCINFO` blob. Faster than shelling
/// out per commit by ~100×.
fn fast_import_fixture(bare: &Path, n: usize) {
    let mut child = Command::new("git")
        .args(["-C", bare.to_str().unwrap(), "fast-import", "--quiet"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .spawn()
        .expect("git fast-import");
    {
        let stdin = child.stdin.as_mut().unwrap();
        for i in 0..n {
            let b = format!("pkg{i:05}");
            let srcinfo = format!("pkgbase = {b}\npkgver = 1\npkgrel = 1\npkgname = {b}\n");
            writeln!(stdin, "blob").unwrap();
            writeln!(stdin, "mark :1").unwrap();
            writeln!(stdin, "data {}", srcinfo.len()).unwrap();
            stdin.write_all(srcinfo.as_bytes()).unwrap();
            writeln!(stdin).unwrap();
            writeln!(stdin, "commit refs/heads/{b}").unwrap();
            writeln!(stdin, "committer t <t@t> 0 +0000").unwrap();
            let msg = format!("c{i}");
            writeln!(stdin, "data {}", msg.len()).unwrap();
            stdin.write_all(msg.as_bytes()).unwrap();
            writeln!(stdin).unwrap();
            writeln!(stdin, "M 100644 :1 .SRCINFO").unwrap();
            writeln!(stdin).unwrap();
        }
        writeln!(stdin, "done").unwrap();
    }
    let status = child.wait().expect("wait fast-import");
    assert!(status.success(), "fast-import failed");
}

/// Build a bare repo whose on-disk shape matches the real AUR mirror:
/// one consolidated packfile + `.idx`, plus a `packed-refs` file
/// holding every branch tip.
fn build_realistic_mirror(root: &Path, n: usize) -> PathBuf {
    let bare = root.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    git(&["init", "-q", "--bare", "-b", "trunk"], &bare);
    fast_import_fixture(&bare, n);
    git(&["-c", "gc.auto=0", "repack", "-ad", "-q"], &bare);
    git(&["pack-refs", "--all"], &bare);
    bare
}

#[test]
fn full_build_does_not_mmap_per_branch() {
    let dir = TempDir::new().unwrap();
    let bare = build_realistic_mirror(dir.path(), N_BRANCHES);

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).expect("open mirror");

    let r0 = getrusage(UsageWho::RUSAGE_SELF).unwrap();
    let started = std::time::Instant::now();
    let idx = full_build(&cfg, &mirror).expect("full_build");
    let elapsed = started.elapsed();
    let r1 = getrusage(UsageWho::RUSAGE_SELF).unwrap();

    let utime_us = (r1.user_time().tv_sec() - r0.user_time().tv_sec()) * 1_000_000
        + (r1.user_time().tv_usec() - r0.user_time().tv_usec());
    let stime_us = (r1.system_time().tv_sec() - r0.system_time().tv_sec()) * 1_000_000
        + (r1.system_time().tv_usec() - r0.system_time().tv_usec());
    let minflt = r1.minor_page_faults() - r0.minor_page_faults();
    let majflt = r1.major_page_faults() - r0.major_page_faults();
    let vcsw = r1.voluntary_context_switches() - r0.voluntary_context_switches();
    let ivcsw = r1.involuntary_context_switches() - r0.involuntary_context_switches();

    eprintln!("=== full_build over {N_BRANCHES} branches ===");
    eprintln!("entries:               {}", idx.entries.len());
    eprintln!("wall:                  {:.3}s", elapsed.as_secs_f64());
    eprintln!("user time (µs):        {utime_us}");
    eprintln!("system time (µs):      {stime_us}");
    eprintln!("minor page faults:     {minflt}");
    eprintln!("major page faults:     {majflt}");
    eprintln!("voluntary ctxt sw:     {vcsw}");
    eprintln!("involuntary ctxt sw:   {ivcsw}");

    assert_eq!(idx.entries.len(), N_BRANCHES);
    assert!(
        minflt < MINFLT_THRESHOLD,
        "minor page faults {minflt} ≥ {MINFLT_THRESHOLD} — \
         workers likely mmap'ing the pack `.idx` per branch \
         (per-iter `gix::open` regression?)",
    );
}
