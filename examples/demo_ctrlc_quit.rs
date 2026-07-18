//! Demo driver: Ctrl-C at the *idle* shell prompt quits aurox — like Ctrl-D,
//! but with the `128 + SIGINT` exit code (130) a wrapper can tell apart from
//! `quit`'s 0. Typed at a real bash prompt so the exit — and the `echo $?`
//! showing 130 — is visible in the recording.
//!
//! The prompt half of the shell's Ctrl-C contract: mid-operation a ^C bails
//! back to the prompt (extended/31: builds, extended/37: AUR refresh,
//! extended/39: repo-DB refresh); at an idle prompt there is nothing left to
//! abort, so ^C leaves the shell.
//!
//! Rendered to `docs/demo/ctrlc-quit.gif` by `demos/build.sh`; run as a
//! plain test by `tests/container/extended/38_demo_ctrlc_quit.sh`.

use pty_harness::{Pty, back_at_prompt, dwell};

fn main() {
    let mut pty = Pty::spawn_demo_shell();
    pty.expect("demo shell prompt", |s| s.contains('\u{276F}'));
    dwell(1000);

    pty.send_human("aurox");
    pty.expect("aurox shell banner", |s| s.contains("aurox shell"));
    dwell(2000);

    // The user's Ctrl-C at the idle prompt: rustyline's raw mode reads the
    // 0x03 as `Interrupted` (no signal involved) and aurox exits — bash's
    // prompt returning is the visible proof.
    pty.send(&[0x03]);
    pty.expect("back at the bash prompt", back_at_prompt);
    dwell(1200);

    // Pin the contract on screen: the 128+SIGINT shell convention.
    pty.send_human("echo $?");
    pty.expect("exit code 130", |s| s.lines().any(|l| l.trim() == "130"));
    dwell(2000);

    pty.send_human("exit");
    pty.finish_clean();
    println!("DEMO_CTRLC_QUIT_OK");
}
