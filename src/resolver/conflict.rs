//! Declared-conflict detection for the staged transaction.
//!
//! pacman's atomic prepare rejects a `conflicts=` at apply — but for an AUR
//! package that's *after* the user approved and the build ran. The shell
//! resolves the whole cart at `add`; [`check`] runs right after, so a staged
//! package that declares `conflicts=X` on a co-present `X` (another staged
//! package, or an installed one not being removed) — with no matching
//! `replaces=X` — rejects the `add` up front. That is the "a cart with
//! conflicting items is impossible" guarantee. Only *declared* conflicts are
//! checked here; file conflicts stay pacman's apply-time backstop (uncheckable
//! before a build).
//!
//! Scope: the declaring side is the **AUR** entries being installed — a
//! `-bin`/`-git` that declares `conflicts=` its counterpart is the realistic
//! cart collision (curated sync repos don't ship mutually-conflicting packages
//! you'd stage together). A repo package that declares a conflict against a
//! staged AUR one is left to pacman's prepare. The `replaces=` exemption matches
//! [`crate::build::upgrade`]'s transparent-upgrade rule: `replaces=` flips
//! pacman's "remove the old one?" default to yes, so the swap is automatic.

use crate::error::{Error, Result};
use crate::names::{PkgName, PkgTarget};
use std::collections::HashSet;
use std::hash::BuildHasher;

/// One package the transaction installs whose `conflicts=` is checked.
pub struct Declarer {
    /// The concrete pkgname pacman installs.
    pub name: PkgName,
    /// Declared `conflicts=` (bare-name matched via [`PkgTarget::refers_to`]).
    pub conflicts: Vec<PkgTarget>,
    /// Declared `replaces=` — a conflict it also replaces is a transparent swap,
    /// not a blocker.
    pub replaces: Vec<PkgTarget>,
}

/// Reject the transaction when a `declarer` conflicts with a co-present package.
///
/// `staged` is every concrete pkgname the plan installs; `is_installed` probes
/// the localdb (injected, so the check stays pure and doesn't clone the ~100k
/// installed names); `removing` is the staged `-R` set — a conflict cleared by
/// removing the other package is fine. A conflict a declarer also `replaces=`
/// is transparent, and a package never conflicts with itself.
pub fn check<S: BuildHasher>(
    declarers: &[Declarer],
    staged: &HashSet<PkgName, S>,
    is_installed: impl Fn(&PkgName) -> bool,
    removing: &HashSet<PkgName, S>,
) -> Result<()> {
    for d in declarers {
        for c in &d.conflicts {
            // A conflict this package also replaces is a transparent swap.
            if d.replaces.iter().any(|r| r.bare() == c.bare()) {
                continue;
            }
            let other = PkgName::from(c.bare());
            // Not itself, and not a package this same transaction removes.
            if other == d.name || removing.contains(&other) {
                continue;
            }
            if staged.contains(&other) || is_installed(&other) {
                return Err(Error::other(format!(
                    "{} conflicts with {other}: both would be present after this transaction — \
                     drop one, or `remove {other}` first",
                    d.name
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Declarer, check};
    use crate::names::{PkgName, PkgTarget};
    use std::collections::HashSet;

    fn declarer(name: &str, conflicts: &[&str], replaces: &[&str]) -> Declarer {
        Declarer {
            name: PkgName::from(name),
            conflicts: conflicts.iter().map(|s| PkgTarget::new(*s)).collect(),
            replaces: replaces.iter().map(|s| PkgTarget::new(*s)).collect(),
        }
    }

    fn names(specs: &[&str]) -> HashSet<PkgName> {
        specs.iter().map(|s| PkgName::from(*s)).collect()
    }

    /// Nothing installed, no removals.
    fn none(_: &PkgName) -> bool {
        false
    }

    #[test]
    fn two_staged_packages_that_conflict_are_rejected() {
        // `foo-bin` (staged) declares conflicts=foo; `foo` is also staged.
        let declarers = [declarer("foo-bin", &["foo"], &[])];
        let staged = names(&["foo-bin", "foo"]);
        let err = check(&declarers, &staged, none, &HashSet::new()).unwrap_err();
        assert!(
            err.to_string().contains("foo-bin conflicts with foo"),
            "{err}"
        );
    }

    #[test]
    fn conflict_with_an_installed_package_is_rejected() {
        let declarers = [declarer("dotnet-core-bin", &["dotnet-runtime"], &[])];
        let staged = names(&["dotnet-core-bin"]);
        let installed = names(&["dotnet-runtime"]);
        let err = check(
            &declarers,
            &staged,
            |n| installed.contains(n),
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("remove dotnet-runtime"), "{err}");
    }

    #[test]
    fn a_matching_replaces_makes_the_conflict_transparent() {
        // conflicts=X paired with replaces=X: the swap is automatic, not a block.
        let declarers = [declarer(
            "dotnet-core-bin",
            &["dotnet-runtime"],
            &["dotnet-runtime"],
        )];
        let installed = names(&["dotnet-runtime"]);
        assert!(
            check(
                &declarers,
                &names(&["dotnet-core-bin"]),
                |n| installed.contains(n),
                &HashSet::new()
            )
            .is_ok()
        );
    }

    #[test]
    fn removing_the_conflicting_package_clears_it() {
        // Staging the -bin while also staging the old one for removal is fine.
        let declarers = [declarer("dotnet-core-bin", &["dotnet-runtime"], &[])];
        let installed = names(&["dotnet-runtime"]);
        let removing = names(&["dotnet-runtime"]);
        assert!(
            check(
                &declarers,
                &names(&["dotnet-core-bin"]),
                |n| installed.contains(n),
                &removing
            )
            .is_ok()
        );
    }

    #[test]
    fn a_conflict_against_something_absent_is_fine() {
        // conflicts=X but X is neither staged nor installed → no collision.
        let declarers = [declarer("foo-bin", &["foo"], &[])];
        assert!(check(&declarers, &names(&["foo-bin"]), none, &HashSet::new()).is_ok());
    }

    #[test]
    fn a_versioned_conflict_matches_on_the_bare_name() {
        // conflicts=foo>=2 collides with a present `foo` (constraint ignored for
        // the bare-name co-presence test).
        let declarers = [declarer("foo-bin", &["foo>=2"], &[])];
        assert!(
            check(
                &declarers,
                &names(&["foo-bin", "foo"]),
                none,
                &HashSet::new()
            )
            .is_err()
        );
    }
}
