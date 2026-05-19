//! Pre-resolve pass: expand pkgbase-only `-S` targets into the pkgnames the
//! user actually wants installed as **explicit**.
//!
//! Background: classification accepts a bare pkgbase (yay-style `-S bisq`
//! when the pkgname is `bisq-desktop`), but the downstream Explicit-vs-deps
//! bookkeeping is pkgname-keyed — `install_stratum` matches a built file's
//! pkgname against `plan.direct_targets`. Without this pass the user's named
//! pkgbase would never match the built pkgnames, so every produced package
//! ends up `--asdeps`.
//!
//! For a split pkgbase, the user may legitimately want only a subset of the
//! pkgnames (`makepkg --pkg=` skips packaging the rest, and we further skip
//! installing them). The actual choice is delegated to a callback so the
//! UI lives in `ui.rs` and tests can inject a deterministic selector.
//!
//! Targets that already resolve as pkgname / provides / pacman are passed
//! through unchanged — preserving any version constraint suffix.

use crate::error::{Error, Result};
use crate::index::secondary::{self, Secondary};
use crate::index::IndexFile;
use crate::pacman::alpm_db::PacmanIndex;
use std::collections::HashMap;
use tracing::{debug, instrument};

/// Selector callback: given a pkgbase and its full pkgname list, return the
/// subset to install as explicit. Boxed via `&mut dyn` at call sites so the
/// signature stays one line and so test/UI variants compose without generics.
pub type PkgnameSelector<'a> = dyn FnMut(&str, &[String]) -> Result<Vec<String>> + 'a;

/// Outcome of [`expand_pkgbase_targets`].
///
/// Targets are kept at **pkgbase granularity** wherever the bare user input
/// matched via the pkgbase or provides path. That avoids the `by_name`
/// collision trap: an unrelated AUR pkgbase can produce a pkgname that
/// collides with one we'd expand to, and `Secondary::by_name` only stores
/// one entry per name (`HashMap` insert-order winner). Pinning to a pkgbase
/// string lets [`super::classify`] route through `by_pkgbase`, which is
/// unique by construction.
#[derive(Debug, Default)]
pub struct ExpandedTargets {
    /// Rewritten target list ready for [`super::resolve`]. May contain the
    /// pkgbase string for rewritten entries; pacman / `by_name` passthroughs
    /// keep their original form (with any version constraint suffix).
    pub targets: Vec<String>,
    /// pkgbase → user-selected pkgnames, populated only when the user kept a
    /// **proper subset** of a split pkgbase. The build pipeline uses this
    /// for the install-side `pacman -U` filter. Pkgbases absent from the
    /// map default to "install every built pkgname".
    pub selections: HashMap<String, Vec<String>>,
    /// pkgnames the user effectively named — extracted from the selector
    /// (for pkgbase hits) or the provider attribution (for provides hits).
    /// `cmd_install` merges this into `Plan.direct_targets` so
    /// `install_stratum` can mark the corresponding `.pkg.tar.zst` as
    /// Explicit rather than `--asdeps`. Empty for pkgname / pacman /
    /// no-index passthroughs — those keep the resolver's existing
    /// "targets become direct" behaviour.
    pub direct_pkgnames: Vec<String>,
}

/// Rewrite `targets`, expanding bare pkgbase / provides references so the
/// resolver receives a stable, by_pkgbase-resolvable name.
///
/// `select(pkgbase, &pkgnames)` is invoked for every target that resolves
/// only through the pkgbase fallback (not pkgname, not provides). It must
/// return a non-empty subset of `pkgnames` to install as explicit.
#[instrument(skip(idx, by, pac, targets, select), fields(targets = targets.len()))]
pub fn expand_pkgbase_targets(
    idx: &IndexFile,
    by: Option<&Secondary>,
    pac: &PacmanIndex,
    targets: &[String],
    select: &mut PkgnameSelector<'_>,
) -> Result<ExpandedTargets> {
    let mut out = ExpandedTargets {
        targets: Vec::with_capacity(targets.len()),
        ..ExpandedTargets::default()
    };

    for t in targets {
        let bare = secondary::strip_version_constraint(t);
        let Some(by) = by else {
            out.targets.push(t.clone());
            continue;
        };
        // pacman & by_name both bypass any AUR rewriting.
        if pac.is_installed(bare) || pac.in_sync(bare) || by.by_name.contains_key(bare) {
            out.targets.push(t.clone());
            continue;
        }
        // Virtual name (`-S bisq` where `bisq-desktop` declares
        // `provides = bisq`): pin the resolver to that pkgbase's entry by
        // passing the pkgbase string — `by_name["bisq-desktop"]` could
        // alias to an unrelated pkgbase that happens to ship the same
        // pkgname, and we'd then build the wrong package.
        if by.by_provides.contains_key(bare) {
            let (entry_idx, pkgname) = by
                .provider_of(idx, bare)
                .expect("by_provides hit must have a provider_of resolution");
            let entry = &idx.entries[entry_idx];
            debug!(
                pkgbase = %entry.pkgbase,
                virtual_name = bare,
                pkgname,
                "rewrote provides target to providing pkgbase",
            );
            // A pkgbase-level `provides` makes every pkgname a provider, so
            // there's no real subset — leave selection unset so every
            // built pkgname reaches `pacman -U`. Only pkgname-scoped
            // provides yield a true single-pkgname subset.
            let scoped = entry.pkgnames.iter().any(|p| {
                p.name == pkgname
                    && p.provides
                        .iter()
                        .any(|x| secondary::strip_version_constraint(x) == bare)
            });
            if scoped && entry.pkgnames.len() > 1 {
                out.selections
                    .insert(entry.pkgbase.clone(), vec![pkgname.to_string()]);
            }
            out.targets.push(entry.pkgbase.clone());
            out.direct_pkgnames.push(pkgname.to_string());
            continue;
        }
        // Bare pkgbase (`-S commit-mono-font`): defer to the selector.
        // Single-pkgname pkgbases auto-pick; split pkgbases prompt.
        if !by.by_pkgbase.contains_key(bare) {
            out.targets.push(t.clone());
            continue;
        }
        let entry_idx = by.by_pkgbase[bare] as usize;
        let entry = &idx.entries[entry_idx];
        let pkgname_strs: Vec<String> = entry.pkgnames.iter().map(|p| p.name.clone()).collect();
        let chosen = select(&entry.pkgbase, &pkgname_strs)?;
        if chosen.is_empty() {
            return Err(Error::other(format!(
                "no pkgnames selected for pkgbase `{}`",
                entry.pkgbase
            )));
        }
        // Sanity: every chosen pkgname must actually belong to this pkgbase.
        // Catches a buggy selector returning unrelated strings before we feed
        // them into the resolver.
        for c in &chosen {
            if !entry.pkgnames.iter().any(|p| &p.name == c) {
                return Err(Error::other(format!(
                    "selector returned `{c}` which is not a pkgname of `{}`",
                    entry.pkgbase
                )));
            }
        }
        debug!(
            pkgbase = %entry.pkgbase,
            available = entry.pkgnames.len(),
            chosen = chosen.len(),
            "expanded pkgbase target",
        );
        // Record selection only when it's a true subset — full selection
        // is the default and doesn't need to constrain `pacman -U`.
        if chosen.len() < entry.pkgnames.len() {
            out.selections.insert(entry.pkgbase.clone(), chosen.clone());
        }
        out.targets.push(entry.pkgbase.clone());
        out.direct_pkgnames.extend(chosen);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::{IndexEntry, Pkgname};

    /// Entry whose `provides` live at the pkgbase level (every pkgname is
    /// a provider — the common AUR shape).
    fn entry(pkgbase: &str, pkgnames: &[&str], provides: &[&str]) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: pkgnames
                .iter()
                .map(|s| Pkgname {
                    name: (*s).into(),
                    provides: Vec::new(),
                })
                .collect(),
            provides: provides.iter().map(|s| (*s).into()).collect(),
            pkgver: "1".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    /// Split pkgbase where one pkgname declares scoped provides (bisq shape).
    fn entry_scoped(
        pkgbase: &str,
        provider_pkgname: &str,
        provider_provides: &[&str],
        siblings: &[&str],
    ) -> IndexEntry {
        let mut pkgnames = vec![Pkgname {
            name: provider_pkgname.into(),
            provides: provider_provides.iter().map(|s| (*s).into()).collect(),
        }];
        for s in siblings {
            pkgnames.push(Pkgname {
                name: (*s).into(),
                provides: Vec::new(),
            });
        }
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames,
            pkgver: "1".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    fn fixture() -> (IndexFile, Secondary, PacmanIndex) {
        let idx = IndexFile {
            entries: vec![
                // Real-world: pkgbase ≠ pkgname (the bisq case, single pkgname).
                entry("bisq-single", &["bisq-desktop-single"], &[]),
                // bisq itself: split pkg where one pkgname provides the virtual.
                entry_scoped(
                    "bisq",
                    "bisq-desktop",
                    &["bisq"],
                    &["bisq-cli", "bisq-daemon"],
                ),
                // Split pkg: pkgbase has multiple pkgnames, no scoped provides.
                entry("split-pkg", &["split-a", "split-b", "split-c"], &[]),
                // Trivial: pkgname == pkgbase.
                entry("cower", &["cower"], &[]),
                // Pkgbase-level provides (every pkgname is implicitly a provider).
                entry("paru-bin", &["paru-bin"], &["paru"]),
            ],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.sync_versions.insert("firefox".into(), "110.0-1".into());
        (idx, by, pac)
    }

    /// Trivial selector — picks every pkgname. Wrapped in `Result` because
    /// the `expand_pkgbase_targets` callback type is `FnMut(...) -> Result<_>`.
    #[allow(clippy::unnecessary_wraps)]
    fn select_all(_: &str, pkgnames: &[String]) -> Result<Vec<String>> {
        Ok(pkgnames.to_vec())
    }

    #[test]
    fn passes_through_pkgname_targets_unchanged() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["bisq-desktop".to_string()],
            &mut select_all,
        )
        .unwrap();
        // by_name hit: passthrough, classifier handles direct_targets itself.
        assert_eq!(r.targets, vec!["bisq-desktop".to_string()]);
        assert!(r.selections.is_empty(), "no selection for pkgname targets");
        assert!(
            r.direct_pkgnames.is_empty(),
            "passthrough → resolver populates direct_targets from `targets`",
        );
    }

    #[test]
    fn scoped_provides_rewrites_to_pkgbase_with_subset() {
        // bisq pkgbase: only `bisq-desktop` declares `provides = bisq`.
        // `-S bisq` rewrites to the *pkgbase* (not the pkgname) so the
        // resolver pins through `by_pkgbase`, dodging by_name collisions
        // with other AUR entries that happen to ship the same pkgname.
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["bisq".to_string()],
            &mut select_all,
        )
        .unwrap();
        assert_eq!(
            r.targets,
            vec!["bisq".to_string()],
            "resolve target is pkgbase"
        );
        assert_eq!(
            r.direct_pkgnames,
            vec!["bisq-desktop".to_string()],
            "the providing pkgname is the user's actual direct target",
        );
        assert_eq!(
            r.selections.get("bisq"),
            Some(&vec!["bisq-desktop".to_string()]),
            "scoped provides must constrain the install-side filter",
        );
    }

    #[test]
    fn pkgbase_level_provides_does_not_constrain_split_pkgbase() {
        // paru-bin has pkgbase-level `provides = paru` on a single-pkgname
        // pkgbase. `-S paru` rewrites to the pkgbase string; no selection
        // recorded because every pkgname implicitly provides the virtual.
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["paru".to_string()],
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["paru-bin".to_string()]);
        assert_eq!(r.direct_pkgnames, vec!["paru-bin".to_string()]);
        assert!(
            r.selections.is_empty(),
            "single-pkgname pkgbase means no real subset",
        );
    }

    #[test]
    fn passes_through_pacman_targets_unchanged() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["firefox".to_string()],
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["firefox".to_string()]);
        assert!(r.direct_pkgnames.is_empty());
    }

    #[test]
    fn expands_bare_pkgbase_to_pkgbase_target_with_chosen_pkgnames() {
        // pkgbase `bisq-single` has only `bisq-desktop-single` as pkgname,
        // no provides — the pure-pkgbase fallback path. Resolve target is
        // the pkgbase; `direct_pkgnames` carries the actual pkgname for
        // install_stratum to mark Explicit.
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["bisq-single".to_string()],
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["bisq-single".to_string()]);
        assert_eq!(r.direct_pkgnames, vec!["bisq-desktop-single".to_string()]);
        // Full selection (1/1): no need to record.
        assert!(r.selections.is_empty());
    }

    #[test]
    fn expands_split_pkgbase_to_all_pkgnames_by_default() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["split-pkg".to_string()],
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["split-pkg".to_string()]);
        assert_eq!(
            r.direct_pkgnames,
            vec![
                "split-a".to_string(),
                "split-b".to_string(),
                "split-c".to_string()
            ],
        );
        assert!(
            r.selections.is_empty(),
            "all-pkgnames selection is the default; no install-side filter needed",
        );
    }

    #[test]
    fn split_pkgbase_partial_selection_records_constraint() {
        let (idx, by, pac) = fixture();
        let mut select = |_pkgbase: &str, _pkgnames: &[String]| -> Result<Vec<String>> {
            Ok(vec!["split-a".to_string(), "split-c".to_string()])
        };
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["split-pkg".to_string()],
            &mut select,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["split-pkg".to_string()]);
        assert_eq!(
            r.direct_pkgnames,
            vec!["split-a".to_string(), "split-c".to_string()],
        );
        assert_eq!(
            r.selections.get("split-pkg"),
            Some(&vec!["split-a".to_string(), "split-c".to_string()]),
            "partial selection records the install-side filter constraint",
        );
    }

    #[test]
    fn pkgname_beats_pkgbase_when_both_could_match() {
        // `cower` matches both by_name and by_pkgbase. pkgname wins → no expand,
        // no selector call.
        let (idx, by, pac) = fixture();
        let mut calls = 0;
        let mut select = |_p: &str, n: &[String]| -> Result<Vec<String>> {
            calls += 1;
            Ok(n.to_vec())
        };
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &["cower".to_string()], &mut select)
            .unwrap();
        assert_eq!(r.targets, vec!["cower".to_string()]);
        assert_eq!(calls, 0, "selector must not be invoked on pkgname hits");
    }

    #[test]
    fn version_constraint_preserved_on_passthrough() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["bisq-desktop>=1.2".to_string()],
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["bisq-desktop>=1.2".to_string()]);
    }

    #[test]
    fn empty_selection_errors() {
        let (idx, by, pac) = fixture();
        let mut select = |_p: &str, _n: &[String]| -> Result<Vec<String>> { Ok(vec![]) };
        let err = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["split-pkg".to_string()],
            &mut select,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("no pkgnames selected"));
    }

    #[test]
    fn selector_returning_unrelated_pkgname_errors() {
        let (idx, by, pac) = fixture();
        let mut select = |_p: &str, _n: &[String]| -> Result<Vec<String>> {
            Ok(vec!["totally-unrelated".to_string()])
        };
        let err = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["split-pkg".to_string()],
            &mut select,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("not a pkgname of"));
    }

    #[test]
    fn pkgbase_target_survives_pkgname_collision_with_another_pkgbase() {
        // Real-world: AUR has `commit-mono-font` (pkgbase, pkgnames
        // otf-commit-mono + ttf-commit-mono) *and* a separate
        // `otf-commit-mono` pkgbase. `by_name["otf-commit-mono"]` only
        // stores one entry (HashMap insert-order winner); the resolver
        // would silently build the wrong pkgbase if expand handed it the
        // pkgname. Fix: expand emits the pkgbase string so classify
        // routes through `by_pkgbase`, which is unique.
        let idx = IndexFile {
            entries: vec![
                entry(
                    "commit-mono-font",
                    &["otf-commit-mono", "ttf-commit-mono"],
                    &[],
                ),
                // Unrelated pkgbase that happens to ship a pkgname matching
                // a sibling of the entry above.
                entry("otf-commit-mono", &["otf-commit-mono"], &[]),
            ],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let pac = PacmanIndex::default();

        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &["commit-mono-font".to_string()],
            &mut select_all,
        )
        .unwrap();
        assert_eq!(
            r.targets,
            vec!["commit-mono-font".to_string()],
            "must pass the pkgbase string, NOT the pkgnames — by_name would alias to the wrong entry",
        );
        // Both pkgnames still flow through `direct_pkgnames` so
        // install_stratum marks them Explicit.
        assert_eq!(
            r.direct_pkgnames,
            vec!["otf-commit-mono".to_string(), "ttf-commit-mono".to_string()],
        );
    }

    #[test]
    fn no_index_means_passthrough() {
        let (idx, _by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            None,
            &pac,
            &["bisq-single".to_string()],
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["bisq-single".to_string()]);
        assert!(r.selections.is_empty());
    }
}
