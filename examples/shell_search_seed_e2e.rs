//! End-to-end driver for the bare-term launch `aurox <term>…`, used by
//! `tests/container/extended/08_shell_search_seed.sh`.
//!
//! `aurox <term>…` interactively opens the shell *seeded* with that search — the
//! same result as starting the shell and typing `search <term>…`. There is no
//! picker any more (the REPL is the one interactive surface). This spawns the
//! real `aurox test-trivial` under a PTY and asserts:
//!
//! ```text
//!   (launch)  → shell caption, then the seeded numbered result list, at a prompt
//!   add 1     → the row is addressable by its number (seeded list remembered)
//!   quit      → clean exit
//! ```
//!
//! A seeded launch shows no ox art — the splash is reserved for a bare `aurox`
//! (its result table would bury the art) — only the `aurox shell …` caption.
//!
//! The `.sh` runs `aurox -Sy` first so the on-disk index can classify
//! `test-trivial` as an AUR package (the shell does not fetch at startup).

use pty_harness::Pty;

fn main() {
    // Launch straight into the seeded search — the exact-name regex keeps the
    // list to the single `test-trivial` fixture so `add 1` is unambiguous.
    let mut pty = Pty::spawn_aurox_args(&["^test-trivial$"]);

    // The shell still prints its caption (the ox art is suppressed on a seeded
    // launch — its result table would bury it — but the one-liner stays)…
    pty.expect("shell caption", |s| s.contains("aurox shell"));
    // …and the seeded search ran before the prompt: the numbered row is on
    // screen without the user typing `search`. The aligned table renders repo +
    // name as separate columns, so the row reads `1  aur   test-trivial …`.
    pty.expect("seeded result row", |s| {
        s.contains("test-trivial") && s.contains("  1")
    });

    // The seeded list is remembered, so the row is addressable by its number —
    // proof the launch went through the same `search` dispatch as typing it.
    pty.send(b"add 1\r");
    pty.expect("staged by number", |s| s.contains("staged test-trivial"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_SEARCH_SEED_E2E_OK");
}
