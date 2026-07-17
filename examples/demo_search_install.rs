//! README hero screencast driver: seeded search → stage → review gate →
//! apply, paced for human viewers (docs/plans/screencasts.md phase 2).
//!
//! Same harness and fixtures as the e2e drivers, but with `send_human` typing
//! and `dwell` reading pauses so the recording plays like a live session.
//! `demos/build.sh` runs it in the container with `PTY_CAST_DIR` set and
//! renders the cast to `docs/demo/search-install.gif`;
//! `tests/container/extended/33_demo_search_install.sh` runs it as a plain
//! test (no recording), so the scripted flow can't rot when the shell UX
//! changes.
//!
//! The expectations mirror `shell_search_seed_e2e` + `shell_cart_e2e`; the
//! flow keeps the gated `apply` on purpose — the refusal-then-approve beat
//! *is* the review-gate story the demo exists to tell. Staging is by name,
//! not row number: the numbered list includes real `[extra]` packages whose
//! order shifts with the baked sync DB, and the demo must survive rebuilds.
//! The install target is the `test-hello` fixture, whose `build()` streams a
//! fake compile log — a real-feeling build instead of a single-frame blip.

use pty_harness::{Pty, dwell};

fn main() {
    // Launch seeded: `aurox hello` opens the shell with that search already
    // run — banner, then a numbered result table mixing repo and AUR rows.
    let mut pty = Pty::spawn_aurox_args(&["hello"]);
    pty.expect("shell banner", |s| s.contains("aurox shell"));
    pty.expect("seeded search results", |s| s.contains("test-hello"));
    dwell(2500);

    pty.send_human("add test-hello");
    pty.expect("staged", |s| s.contains("staged test-hello"));
    dwell(1800);

    // The gate refuses to install an unreviewed AUR package…
    pty.send_human("apply");
    pty.expect("review gate", |s| s.contains("needs review"));
    dwell(2000);

    // …and clears after an explicit approve.
    pty.send_human("approve test-hello");
    pty.expect("approved", |s| s.contains("approved test-hello"));
    dwell(1400);

    pty.send_human("apply");
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    dwell(1500);
    pty.send(b"\r");
    pty.expect("apply finished", |s| s.contains("done"));
    dwell(2500);

    pty.send_human("quit");
    pty.finish_clean();
    println!("DEMO_SEARCH_INSTALL_OK");
}
