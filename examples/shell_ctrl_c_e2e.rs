//! End-to-end driver for Ctrl-C during a shell `apply` build, used by
//! `tests/container/extended/31_shell_ctrl_c_bails_to_prompt.sh`.
//!
//! test-sleep-build's `build()` prints a sentinel then sleeps. Once the
//! sentinel is on screen, ^C goes down the PTY — the real keystroke, so the
//! kernel delivers SIGINT to aurox's foreground process group. aurox must
//! catch it (extended/02 pins the forward-to-makepkg mechanics on the `-S`
//! path), mark the build interrupted, fold the empty report — cart kept for
//! retry — and land back at a live prompt; `quit` then exits 0. The `.sh`
//! asserts nothing installed and no orphaned sleep survived.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    pty.send(b"add test-sleep-build\r");
    pty.expect("staged test-sleep-build", |s| {
        s.contains("staged test-sleep-build")
    });
    pty.send(b"approve *\r");
    pty.expect("approved", |s| s.contains("approved test-sleep-build"));

    pty.send(b"apply\r");
    pty.expect("build started (sentinel)", |s| {
        s.contains("AUROX_SLEEP_BUILD_SENTINEL")
    });

    // The user's Ctrl-C, as the terminal would deliver it.
    pty.send(&[0x03]);
    pty.expect("interrupt marked", |s| s.contains("build interrupted"));
    pty.expect("cart kept for retry", |s| {
        s.contains("apply failed — nothing installed; cart kept for retry")
    });

    // Back at a live prompt: a clean quit is the proof (a shell killed by the
    // SIGINT would exit non-zero and fail finish_clean).
    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_CTRL_C_E2E_OK");
}
