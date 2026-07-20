//! README hero screencast driver: bare launch (ox splash) → typed search →
//! stage → review gate → apply, paced for human viewers
//! (docs/plans/screencasts.md phase 2).
//!
//! Same harness and fixtures as the e2e drivers, but with `send_human` typing
//! and `dwell` reading pauses so the recording plays like a live session.
//! `demos/build.sh` runs it in the container with `PTY_CAST_DIR` set and
//! renders the cast to `docs/demo/search-install.gif`;
//! `tests/container/extended/33_demo_search_install.sh` runs it as a plain
//! test (no recording), so the scripted flow can't rot when the shell UX
//! changes.
//!
//! The hero opens with a *bare* `aurox` so the ox splash greets the viewer:
//! the splash is the bare launch's alone — a seeded `aurox hello` would skip it
//! (its result table would bury the art), so the search is typed at the prompt
//! instead. The rest mirrors `shell_cart_e2e`; the flow keeps the gated `apply`
//! on purpose — the refusal-then-approve beat *is* the review-gate story the
//! demo exists to tell. Staging is by name, not row number: the numbered list
//! includes real `[extra]` packages whose order shifts with the baked sync DB,
//! and the demo must survive rebuilds. The install target is the `test-hello`
//! fixture, whose `build()` streams a fake compile log — a real-feeling build
//! instead of a single-frame blip.

use pty_harness::{Pty, back_at_prompt, dwell};

fn main() {
    // Type the launch at a real prompt — the viewer sees `aurox` itself, not a
    // session that starts mid-air. A *bare* launch is what greets with the ox
    // splash (a seeded `aurox hello` would skip it — its result table would bury
    // the art), so the hero opens bare to show the ox, then searches.
    let mut pty = Pty::spawn_demo_shell();
    pty.expect("demo shell prompt", |s| s.contains('\u{276F}'));
    dwell(1000);
    pty.send_human("aurox");
    // The ox greets first — its `(oo)` eyes are a stable literal from the art —
    // then the caption one-liner, which is also the ack that the shell is at its
    // prompt (see `shell_cart_e2e`).
    pty.expect("ox splash", |s| s.contains("(oo)"));
    pty.expect("shell caption", |s| s.contains("aurox shell"));
    dwell(2000);

    // A typed search fills the numbered result table mixing repo and AUR rows.
    pty.send_human("search hello");
    pty.expect("search results", |s| s.contains("test-hello"));
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
    pty.expect("back at the bash prompt", back_at_prompt);
    dwell(1200);
    pty.send_human("exit");
    pty.finish_clean();
    println!("DEMO_SEARCH_INSTALL_OK");
}
