//! End-to-end driver for the inline AUR-setup offer on `-S`, used by
//! `tests/container/smoke/65_install_offer.sh`.
//!
//! `aurox -S test-trivial` on a never-synced state can't resolve the name;
//! instead of dying with "unknown target(s)" it offers the one-time mirror
//! setup (TTY only — see `RefreshReason::InstallOffer`), bootstraps on "y",
//! and retries the same install:
//!
//! ```text
//!   (launch) → "may be in the AUR" note + cost announcement + Y/n prompt
//!   y        → clone + index build, then the install plan for test-trivial
//!   (build)  → makepkg + pacman -U run unattended to a clean exit
//! ```
//!
//! The `.sh` sets `review_default = "skip"` so the offer prompt is the only
//! interactive stop, then asserts the package actually landed.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_aurox_args(&["-S", "test-trivial"]);

    pty.expect("offer note", |s| s.contains("may be in the AUR"));
    pty.expect("cost announcement", |s| s.contains("first-time AUR setup"));
    pty.expect("consent question", |s| {
        s.contains("clone the AUR mirror now?")
    });
    pty.send(b"y");

    // Bootstrap runs (clone + index), then the retry resolves the target into
    // a build-order plan — the moment the offer's promise is kept.
    pty.expect("index built", |s| s.contains("index built"));
    pty.expect("retried plan", |s| s.contains("AUR build order"));

    // The per-pkgbase PKGBUILD review still gates the build (only --noconfirm
    // collapses it); Enter takes the default action, approve.
    pty.expect("review prompt", |s| s.contains("review —"));
    pty.send(b"\r");

    // Build + install run unattended; finish_clean waits for the exit status.
    pty.finish_clean();
    println!("INSTALL_OFFER_E2E_OK");
}
