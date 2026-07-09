//! End-to-end driver for the interactive shell's number-follows-the-shown-list
//! resolution plus `undo`/`redo`, used by
//! `tests/container/extended/09_shell_undo.sh`.
//!
//! The shell only runs interactively (stdin must be a TTY), so this spawns the
//! real no-arg `aurox` under a PTY (via the shared [`pty_harness::Pty`]) and
//! scripts the cart-editing flow against two AUR fixtures. It never `apply`s —
//! the whole point is to exercise pure cart edits — so the `.sh` asserts nothing
//! was installed:
//!
//! ```text
//!   add test-trivial test-epoch → both staged (cart sorted: test-epoch, then test-trivial)
//!   remove 1                    → refused: row 1 is a staged install, pointed at `drop`
//!   keep test-trivial           → drops test-epoch
//!   undo                        → the drop is reverted
//!   redo                        → the drop is reapplied
//!   keep test-epoch             → nothing matches (redo really re-dropped it)
//!   quit                        → clean exit
//! ```
//!
//! Each asserted line is unique across the session, so the PTY's screen-diff
//! `expect` can't match a stale earlier line. The exact cart-state transitions
//! are pinned by the unit tests in `src/cli/shell.rs`; this proves the commands
//! are wired through the real REPL and print the right thing.
//!
//! The `.sh` runs `aurox -Sy` first so the shell's on-disk index can classify
//! `test-trivial` and `test-epoch` as AUR (the shell does not fetch at startup).

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_aurox();

    // The shell starts at its prompt; the index was built by the `.sh`'s `-Sy`.
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // Stage two AUR fixtures. The cart sorts by spec, so row 1 is `test-epoch`.
    pty.send(b"add test-trivial test-epoch\r");
    pty.expect("both staged", |s| s.contains("staged test-epoch"));

    // `remove 1` lands on a staged install — you can't uninstall what isn't
    // installed yet, so it's refused and pointed at `drop` (the reported bug was
    // this erroring with "run search first" against an empty search list).
    pty.send(b"remove 1\r");
    pty.expect("remove refuses a staged install", |s| {
        s.contains("is staged for install")
    });

    // Narrow the cart to one package — the classic over-eager `keep`.
    pty.send(b"keep test-trivial\r");
    pty.expect("keep dropped the other row", |s| {
        s.contains("dropped test-epoch")
    });

    // `undo` brings the dropped package back (the reported "no way to get it
    // back" gap).
    pty.send(b"undo\r");
    pty.expect("undo ran", |s| s.contains("undone"));

    // `redo` reapplies the `keep`.
    pty.send(b"redo\r");
    pty.expect("redo ran", |s| s.contains("redone"));

    // After the redo, `test-epoch` is out of the cart again — so a `keep` on it
    // matches nothing. Proves the redo actually reapplied the drop, not just
    // printed a message.
    pty.send(b"keep test-epoch\r");
    pty.expect("redo really re-dropped test-epoch", |s| {
        s.contains("nothing in the cart matched")
    });

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_UNDO_E2E_OK");
}
