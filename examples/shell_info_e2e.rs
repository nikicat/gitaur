//! End-to-end driver for the shell's `info` source routing, used by
//! `tests/container/extended/10_shell_info_repo.sh`.
//!
//! `info` must describe a sync-repo package from the sync DBs and an AUR
//! package from the index — and the repo lookup runs first, so a name pacman
//! owns never shows an AUR entry. The tie is real: the AUR index also
//! resolves `repo-base`, through `test-provides-repo-base`'s
//! `provides=('repo-base=9.0')`, so an index-first lookup would print that
//! provider's block instead of the sync repo's. The shell is interactive
//! (stdin must be a TTY), so this spawns the real no-arg `gaur` under a PTY:
//!
//! ```text
//!   info repo-base     → `Repository      : local-repo` block (sync-DB hit;
//!                        never the AUR provider's block)
//!   info test-trivial  → `Repository      : aur` block (index hit)
//!   quit               → clean exit
//! ```
//!
//! The `.sh` runs `gaur -Sy` first so the AUR half has an index to answer
//! from; the repo half must work regardless.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_gaur();
    pty.expect("shell banner", |s| s.contains("gitaur shell"));

    // A name both sources resolve: the sync repo owns `repo-base` while the
    // AUR index reaches it via test-provides-repo-base's provides=. Repo must
    // win — an index-first lookup would print the provider's block.
    pty.send(b"info repo-base\r");
    pty.expect("repo info block", |s| {
        s.contains("Repository      : local-repo") && s.contains("Name            : repo-base")
    });
    // `info` prints one block per target, synchronously — with the repo block
    // on screen the command is done, so the provider must not have appeared.
    assert!(
        !pty.screen().contains("test-provides-repo-base"),
        "info repo-base leaked the AUR provider's block\n--- screen ---\n{}",
        pty.screen()
    );

    // An AUR-only package still routes to the index.
    pty.send(b"info test-trivial\r");
    pty.expect("aur info block", |s| {
        s.contains("Repository      : aur") && s.contains("Name            : test-trivial")
    });

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_INFO_E2E_OK");
}
