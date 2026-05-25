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

use crate::build::Target;
use crate::error::{Error, Result};
use crate::index::IndexFile;
use crate::index::schema::IndexEntry;
use crate::index::secondary::{self, Secondary};
use crate::names::{PkgBase, PkgName};
use crate::pacman::alpm_db::PacmanIndex;
use std::collections::HashMap;
use tracing::{debug, instrument};

/// Selector callback: pick the subset of pkgnames to install as explicit.
///
/// Given a pkgbase and its full pkgname list, returns which ones the user
/// wants. Boxed via `&mut dyn` at call sites so the signature stays one
/// line and so test/UI variants compose without generics.
pub type PkgnameSelector<'a> = dyn FnMut(&PkgBase, &[PkgName]) -> Result<Vec<PkgName>> + 'a;

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
    /// Stays `Vec<String>` because the contents are deliberately mixed —
    /// passthroughs can be virtual names, version-constrained pkgnames, or
    /// pacman targets, none of which fits cleanly under `PkgName` or
    /// `PkgBase`.
    pub targets: Vec<String>,
    /// pkgbase → user-selected pkgnames, populated only when the user kept a
    /// **proper subset** of a split pkgbase. The build pipeline uses this
    /// for the install-side `pacman -U` filter. Pkgbases absent from the
    /// map default to "install every built pkgname".
    pub selections: HashMap<PkgBase, Vec<PkgName>>,
    /// pkgnames the user effectively named — extracted from the selector
    /// (for pkgbase hits) or the provider attribution (for provides hits).
    /// `cmd_install` merges this into `Plan.direct_targets` so
    /// `install_stratum` can mark the corresponding `.pkg.tar.zst` as
    /// Explicit rather than `--asdeps`. Empty for pkgname / pacman /
    /// no-index passthroughs — those keep the resolver's existing
    /// "targets become direct" behaviour.
    pub direct_pkgnames: Vec<PkgName>,
    /// pkgbase → user's intended counterpart pkgname. Populated whenever a
    /// target gets rewritten to a pkgbase: the user's typed spec (or the
    /// `-Syu` picker's explicit `Target::hint`) becomes the bias for
    /// [`PacmanIndex::counterpart_with_hint`] inside `prepare_one`. This
    /// is the fix for the dotnet-runtime regression — without it, a pkgbase
    /// that declares many `provides=` virtuals would pick the wrong
    /// installed pkg as the counterpart.
    ///
    /// Empty for pkgbase-only inputs (`-S commit-mono-font`) where the user
    /// didn't type a pkgname; pacman / `by_name` passthroughs don't populate
    /// either (single-pkgname pkgbases get a direct Pkgname match without
    /// help). One hint per pkgbase: when multiple targets rewrite to the
    /// same pkgbase the first one wins, since later ones can only further
    /// disambiguate the same lookup.
    pub counterpart_hints: HashMap<PkgBase, PkgName>,
}

/// Rewrite `targets`, expanding bare pkgbase / provides references so the
/// resolver receives a stable, by_pkgbase-resolvable name.
///
/// Each [`Target`] carries an optional [`Target::hint`] — the user's intended
/// counterpart pkgname. For inputs from the `-Syu` picker the hint is
/// already set (the foreign pkgname that triggered the upgrade); for `-S`
/// inputs it's `None` and the expansion derives one from the spec when it
/// rewrites. Either way, a pkgbase that gets rewritten ends up with an
/// entry in [`ExpandedTargets::counterpart_hints`].
///
/// `select(pkgbase, &pkgnames)` is invoked for every target that resolves
/// only through the pkgbase fallback (not pkgname, not provides). It must
/// return a non-empty subset of `pkgnames` to install as explicit.
#[instrument(skip(idx, by, pac, targets, select), fields(targets = targets.len()))]
pub fn expand_pkgbase_targets(
    idx: &IndexFile,
    by: Option<&Secondary>,
    pac: &PacmanIndex,
    targets: &[Target],
    select: &mut PkgnameSelector<'_>,
) -> Result<ExpandedTargets> {
    let mut out = ExpandedTargets {
        targets: Vec::with_capacity(targets.len()),
        ..ExpandedTargets::default()
    };

    for t in targets {
        let bare = secondary::strip_version_constraint(&t.spec);
        let Some(by) = by else {
            out.targets.push(t.spec.clone());
            continue;
        };
        // Hint recording runs *unconditionally* — the rewrite decision
        // below may short-circuit (pacman wins, passthrough, …), but the
        // resolver still classifies the spec via `resolve_target_source`
        // and can land on an AUR pkgbase even when expand did no rewrite.
        // The dotnet-runtime regression: a foreign virtual that's installed
        // hits `pac.is_installed` → passes through → resolver routes to
        // pkgbase via `by_provides` → without a hint, counterpart picks the
        // first declared provides (wrong). Record the hint here so it's in
        // the map regardless of what expand decides next.
        record_target_hint(by, idx, t, bare, &mut out.counterpart_hints);

        // Per-branch deciders return a TargetDecision; no `&mut` flows into
        // the branches themselves. Order matches the resolver's fallback
        // chain in `resolve_target_source`.
        let decision = if pac.is_installed(bare) || pac.in_sync(bare) {
            decide_pacman_wins(idx, by, t, bare)
        } else if by.by_name.contains_key(bare) {
            decide_pkgname(idx, by, t, bare)
        } else if by.by_provides.contains_key(bare) {
            decide_virtual(idx, by, bare)
        } else if by.by_pkgbase.contains_key(bare) {
            decide_pkgbase(idx, by, bare, select)?
        } else {
            TargetDecision::passthrough(t.spec.clone())
        };

        out.targets.push(decision.spec);
        if let Some((pkgbase, chosen)) = decision.selection {
            extend_selection(&mut out.selections, &pkgbase, &chosen);
        }
        out.direct_pkgnames.extend(decision.direct_pkgnames);
    }
    Ok(out)
}

/// What one target rewrites to. Returned by the per-branch `decide_*`
/// helpers so the main loop applies all three outputs in one place — no
/// `&mut` flows into the branches.
struct TargetDecision {
    /// Pushed into [`ExpandedTargets::targets`]. Either the rewritten
    /// pkgbase string or the original spec for passthrough cases.
    spec: String,
    /// `(pkgbase, chosen)` for [`extend_selection`] when this target
    /// constrains a split pkgbase's install set; `None` otherwise.
    selection: Option<(PkgBase, Vec<PkgName>)>,
    /// Pkgnames to append to [`ExpandedTargets::direct_pkgnames`] — the
    /// user-named ones we flip from `--asdeps` to Explicit later.
    direct_pkgnames: Vec<PkgName>,
}

impl TargetDecision {
    const fn passthrough(spec: String) -> Self {
        Self {
            spec,
            selection: None,
            direct_pkgnames: vec![],
        }
    }
}

/// pacman wins outright — never reroute a name pacman can satisfy. BUT:
/// when `bare` is a pkgname in a multi-pkgname AUR pkgbase (the
/// foreign-installed split-pkg case, e.g. `-Syu` picks
/// `google-cloud-cli-bq` whose pkgbase ships four other pkgnames), we must
/// STILL record the selection here. Otherwise `install_stratum`'s `pacman
/// -U` has no filter and installs every sibling makepkg packaged from the
/// same PKGBUILD. The bisq-cli regression's twin: that one fired through
/// the rewrite branch, this one fires through the shortcut.
fn decide_pacman_wins(idx: &IndexFile, by: &Secondary, t: &Target, bare: &str) -> TargetDecision {
    let selection = by.by_name.get(bare).and_then(|&entry_idx| {
        let entry = &idx.entries[entry_idx as usize];
        (entry.pkgnames.len() > 1).then(|| {
            let bare_name = PkgName::from(bare);
            let chosen = chosen_with_sibling_deps(entry, &bare_name);
            (entry.pkgbase.clone(), chosen)
        })
    });
    TargetDecision {
        spec: t.spec.clone(),
        selection,
        direct_pkgnames: vec![],
    }
}

/// Pkgname hit. Single-pkgname pkgbases pass through unchanged; multi-
/// pkgname pkgbases must rewrite to the pkgbase string AND record a
/// selection, otherwise `install_stratum` has no way to skip the sibling
/// `.pkg.tar.zst` files makepkg always produces from a split PKGBUILD (the
/// bisq-cli regression: `-S bisq-cli` installed bisq-daemon + bisq-desktop
/// too). Hint recorded earlier by `record_target_hint`.
fn decide_pkgname(idx: &IndexFile, by: &Secondary, t: &Target, bare: &str) -> TargetDecision {
    let entry_idx = by.by_name[bare] as usize;
    let entry = &idx.entries[entry_idx];
    if entry.pkgnames.len() == 1 {
        return TargetDecision::passthrough(t.spec.clone());
    }
    let bare_name = PkgName::from(bare);
    let chosen = chosen_with_sibling_deps(entry, &bare_name);
    debug!(
        pkgbase = %entry.pkgbase,
        pkgname = %bare_name,
        chosen = chosen.len(),
        "rewrote split-pkg pkgname target to pkgbase with selection",
    );
    // `into_inner` on the clone is the dedicated PkgBase→String downgrade,
    // used only at this resolver/string boundary (`out.targets` is the
    // mixed-bag `Vec<String>` — see [`ExpandedTargets::targets`]).
    TargetDecision {
        spec: entry.pkgbase.clone().into_inner(),
        selection: Some((entry.pkgbase.clone(), chosen)),
        direct_pkgnames: vec![bare_name],
    }
}

/// Virtual name (`-S bisq` where `bisq-desktop` declares `provides = bisq`):
/// pin the resolver to the providing pkgbase by passing the pkgbase string
/// — `by_name["bisq-desktop"]` could alias to an unrelated pkgbase that
/// happens to ship the same pkgname, and we'd then build the wrong package.
///
/// Selection only when the provides is pkgname-scoped: a pkgbase-level
/// provides makes every pkgname a provider, leaving no real subset.
fn decide_virtual(idx: &IndexFile, by: &Secondary, bare: &str) -> TargetDecision {
    let (entry_idx, pkgname) = by
        .provider_of(idx, bare)
        .expect("by_provides hit must have a provider_of resolution");
    let entry = &idx.entries[entry_idx];
    debug!(
        pkgbase = %entry.pkgbase,
        virtual_name = bare,
        pkgname = %pkgname,
        "rewrote provides target to providing pkgbase",
    );
    let scoped = entry.pkgnames.iter().any(|p| {
        &p.name == pkgname
            && p.provides
                .iter()
                .any(|x| secondary::strip_version_constraint(x) == bare)
    });
    let selection = (scoped && entry.pkgnames.len() > 1).then(|| {
        let chosen = chosen_with_sibling_deps(entry, pkgname);
        (entry.pkgbase.clone(), chosen)
    });
    TargetDecision {
        spec: entry.pkgbase.clone().into_inner(),
        selection,
        direct_pkgnames: vec![pkgname.clone()],
    }
}

/// Bare pkgbase (`-S commit-mono-font`): defer to the selector. Single-
/// pkgname pkgbases auto-pick; split pkgbases prompt. Validates that the
/// selector returned a non-empty subset of this pkgbase's pkgnames before
/// feeding it to the resolver. Hint isn't derived here — a bare pkgbase
/// spec has nothing to map to a counterpart pkgname; the only hint that
/// ends up recorded is an explicit `Target::hint`.
fn decide_pkgbase(
    idx: &IndexFile,
    by: &Secondary,
    bare: &str,
    select: &mut PkgnameSelector<'_>,
) -> Result<TargetDecision> {
    let entry_idx = by.by_pkgbase[bare] as usize;
    let entry = &idx.entries[entry_idx];
    let pkgnames: Vec<PkgName> = entry.pkgnames.iter().map(|p| p.name.clone()).collect();
    let chosen = select(&entry.pkgbase, &pkgnames)?;
    if chosen.is_empty() {
        return Err(Error::other(format!(
            "no pkgnames selected for pkgbase `{}`",
            entry.pkgbase
        )));
    }
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
    // Record selection only when it's a true subset — full selection is
    // the default and doesn't need to constrain `pacman -U`.
    let selection =
        (chosen.len() < entry.pkgnames.len()).then(|| (entry.pkgbase.clone(), chosen.clone()));
    Ok(TargetDecision {
        spec: entry.pkgbase.clone().into_inner(),
        selection,
        direct_pkgnames: chosen,
    })
}

/// First-write-wins on the hints map: multiple targets in the same invocation
/// may rewrite to the same pkgbase (e.g. -S bisq-cli bisq-daemon), and
/// `prepare_one` only consults a single hint per pkgbase. Keep the earliest
/// recorded one — semantically identical because counterpart resolution is
/// pkgname-scoped and either hint would land on a different installed
/// sibling of the same pkgbase.
fn record_hint(hints: &mut HashMap<PkgBase, PkgName>, pkgbase: &PkgBase, hint: PkgName) {
    hints.entry(pkgbase.clone()).or_insert(hint);
}

/// Find which AUR pkgbase a target would route to (`by_name` / `by_provides` /
/// `by_pkgbase`, in that order — mirroring `resolve_target_source`'s fallback
/// chain), then record the hint there.
///
/// Kept independent of the rewrite-decision branches above because the two
/// concerns diverge: a target may route to a pkgbase via the resolver
/// (`by_provides`) without expand itself rewriting (the `pac.is_installed`
/// passthrough). Doing this lookup once at the top of the loop guarantees
/// every routable target gets its hint recorded, regardless of which
/// rewrite branch (if any) the spec ends up in.
///
/// Hint precedence: explicit `Target::hint` always wins. Otherwise we
/// derive one from the spec — but only when the spec is itself a pkgname
/// or virtual (those identities ARE counterpart pkgnames). A bare pkgbase
/// spec without an explicit hint yields no derived hint, because the
/// pkgbase string isn't a counterpart name.
fn record_target_hint(
    by: &Secondary,
    idx: &IndexFile,
    target: &Target,
    bare: &str,
    hints: &mut HashMap<PkgBase, PkgName>,
) {
    let (dest_pkgbase, spec_is_counterpart_name) = if let Some(&entry_idx) = by.by_name.get(bare) {
        (&idx.entries[entry_idx as usize].pkgbase, true)
    } else if by.by_provides.contains_key(bare) {
        let Some((entry_idx, _)) = by.provider_of(idx, bare) else {
            return;
        };
        (&idx.entries[entry_idx].pkgbase, true)
    } else if let Some(&entry_idx) = by.by_pkgbase.get(bare) {
        (&idx.entries[entry_idx as usize].pkgbase, false)
    } else {
        return;
    };
    let hint = target
        .hint
        .clone()
        .or_else(|| spec_is_counterpart_name.then(|| PkgName::from(bare)));
    if let Some(h) = hint {
        record_hint(hints, dest_pkgbase, h);
    }
}

/// Merge `additions` into the per-pkgbase selection list, deduping. Multiple
/// targets in the same `gitaur -S` invocation may reference different
/// pkgnames of the same split pkgbase (`-S bisq-cli bisq-daemon`); each
/// must extend the selection rather than overwrite it.
fn extend_selection(
    selections: &mut HashMap<PkgBase, Vec<PkgName>>,
    pkgbase: &PkgBase,
    additions: &[PkgName],
) {
    let bucket = selections.entry(pkgbase.clone()).or_default();
    for a in additions {
        if !bucket.iter().any(|s| s == a) {
            bucket.push(a.clone());
        }
    }
}

/// Sibling pkgnames of `pkgname` that appear in the pkgbase's pooled
/// `depends`. These are intra-split runtime deps the .SRCINFO parser
/// flattened into a single list — without per-pkgname attribution, we
/// can't tell which sibling owns which dep, so any sibling appearing in
/// the pool is conservatively pulled into the install selection.
///
/// Returns owned `PkgName`s — the `HashSet` ownership lets us probe by
/// `&str` via `Borrow<str>` (`siblings.get(bare)`) instead of routing the
/// comparison through a manual deref dance.
fn sibling_runtime_deps(entry: &IndexEntry, pkgname: &PkgName) -> Vec<PkgName> {
    let siblings: std::collections::HashSet<PkgName> = entry
        .pkgnames
        .iter()
        .map(|p| p.name.clone())
        .filter(|n| n != pkgname)
        .collect();
    entry
        .depends
        .iter()
        .filter_map(|d| {
            siblings
                .get(secondary::strip_version_constraint(d))
                .cloned()
        })
        .collect()
}

/// `pkgname` plus its sibling intra-split runtime deps, deduped. Used by
/// both the `by_name` and scoped-provides paths to compute the install
/// selection for a split pkgbase whose target is a single pkgname.
fn chosen_with_sibling_deps(entry: &IndexEntry, pkgname: &PkgName) -> Vec<PkgName> {
    let mut chosen = vec![pkgname.clone()];
    for sib in sibling_runtime_deps(entry, pkgname) {
        if !chosen.iter().any(|c| c == &sib) {
            chosen.push(sib);
        }
    }
    chosen
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
                    pkgdesc: None,
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
            pkgdesc: None,
        }];
        for s in siblings {
            pkgnames.push(Pkgname {
                name: (*s).into(),
                provides: Vec::new(),
                pkgdesc: None,
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
    fn select_all(_: &PkgBase, pkgnames: &[PkgName]) -> Result<Vec<PkgName>> {
        Ok(pkgnames.to_vec())
    }

    /// Test helper: wrap bare spec strings as hint-less `Target`s — the
    /// shape of `-S <name>` argv where `expand_pkgbase_targets` is expected
    /// to derive any hint from the spec itself.
    fn ts(specs: &[&str]) -> Vec<Target> {
        specs.iter().copied().map(Target::bare).collect()
    }

    #[test]
    fn scoped_provides_rewrites_to_pkgbase_with_subset() {
        // bisq pkgbase: only `bisq-desktop` declares `provides = bisq`.
        // `-S bisq` rewrites to the *pkgbase* (not the pkgname) so the
        // resolver pins through `by_pkgbase`, dodging by_name collisions
        // with other AUR entries that happen to ship the same pkgname.
        let (idx, by, pac) = fixture();
        let r =
            expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["bisq"]), &mut select_all).unwrap();
        assert_eq!(
            r.targets,
            vec!["bisq".to_owned()],
            "resolve target is pkgbase"
        );
        assert_eq!(
            r.direct_pkgnames,
            vec![PkgName::from("bisq-desktop")],
            "the providing pkgname is the user's actual direct target",
        );
        assert_eq!(
            r.selections.get("bisq"),
            Some(&vec![PkgName::from("bisq-desktop")]),
            "scoped provides must constrain the install-side filter",
        );
    }

    #[test]
    fn pkgbase_level_provides_does_not_constrain_split_pkgbase() {
        // paru-bin has pkgbase-level `provides = paru` on a single-pkgname
        // pkgbase. `-S paru` rewrites to the pkgbase string; no selection
        // recorded because every pkgname implicitly provides the virtual.
        let (idx, by, pac) = fixture();
        let r =
            expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["paru"]), &mut select_all).unwrap();
        assert_eq!(r.targets, vec!["paru-bin".to_owned()]);
        assert_eq!(r.direct_pkgnames, vec![PkgName::from("paru-bin")]);
        assert!(
            r.selections.is_empty(),
            "single-pkgname pkgbase means no real subset",
        );
    }

    #[test]
    fn passes_through_pacman_targets_unchanged() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["firefox"]), &mut select_all)
            .unwrap();
        assert_eq!(r.targets, vec!["firefox".to_owned()]);
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
            &ts(&["bisq-single"]),
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["bisq-single".to_owned()]);
        assert_eq!(
            r.direct_pkgnames,
            vec![PkgName::from("bisq-desktop-single")]
        );
        // Full selection (1/1): no need to record.
        assert!(r.selections.is_empty());
    }

    #[test]
    fn expands_split_pkgbase_to_all_pkgnames_by_default() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["split-pkg"]), &mut select_all)
            .unwrap();
        assert_eq!(r.targets, vec!["split-pkg".to_owned()]);
        assert_eq!(
            r.direct_pkgnames,
            vec![
                PkgName::from("split-a"),
                PkgName::from("split-b"),
                PkgName::from("split-c"),
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
        let mut select = |_pkgbase: &PkgBase, _pkgnames: &[PkgName]| -> Result<Vec<PkgName>> {
            Ok(vec![PkgName::from("split-a"), PkgName::from("split-c")])
        };
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["split-pkg"]), &mut select)
            .unwrap();
        assert_eq!(r.targets, vec!["split-pkg".to_owned()]);
        assert_eq!(
            r.direct_pkgnames,
            vec![PkgName::from("split-a"), PkgName::from("split-c")],
        );
        assert_eq!(
            r.selections.get("split-pkg"),
            Some(&vec![PkgName::from("split-a"), PkgName::from("split-c")]),
            "partial selection records the install-side filter constraint",
        );
    }

    #[test]
    fn pkgname_beats_pkgbase_when_both_could_match() {
        // `cower` matches both by_name and by_pkgbase. pkgname wins → no expand,
        // no selector call.
        let (idx, by, pac) = fixture();
        let mut calls = 0;
        let mut select = |_p: &PkgBase, n: &[PkgName]| -> Result<Vec<PkgName>> {
            calls += 1;
            Ok(n.to_vec())
        };
        let r =
            expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["cower"]), &mut select).unwrap();
        assert_eq!(r.targets, vec!["cower".to_owned()]);
        assert_eq!(calls, 0, "selector must not be invoked on pkgname hits");
    }

    #[test]
    fn version_constraint_preserved_on_passthrough() {
        // Single-pkgname pkgbase (`cower`) stays in the passthrough lane.
        // Multi-pkgname pkgnames now rewrite to pkgbase and would drop the
        // version constraint suffix; only single-pkgname / pacman / no-AUR
        // passthroughs preserve it.
        let (idx, by, pac) = fixture();
        let r =
            expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["cower>=1.2"]), &mut select_all)
                .unwrap();
        assert_eq!(r.targets, vec!["cower>=1.2".to_owned()]);
    }

    #[test]
    fn empty_selection_errors() {
        let (idx, by, pac) = fixture();
        let mut select = |_p: &PkgBase, _n: &[PkgName]| -> Result<Vec<PkgName>> { Ok(vec![]) };
        let err = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["split-pkg"]), &mut select)
            .unwrap_err();
        assert!(format!("{err}").contains("no pkgnames selected"));
    }

    #[test]
    fn selector_returning_unrelated_pkgname_errors() {
        let (idx, by, pac) = fixture();
        let mut select = |_p: &PkgBase, _n: &[PkgName]| -> Result<Vec<PkgName>> {
            Ok(vec![PkgName::from("totally-unrelated")])
        };
        let err = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["split-pkg"]), &mut select)
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
            &ts(&["commit-mono-font"]),
            &mut select_all,
        )
        .unwrap();
        assert_eq!(
            r.targets,
            vec!["commit-mono-font".to_owned()],
            "must pass the pkgbase string, NOT the pkgnames — by_name would alias to the wrong entry",
        );
        // Both pkgnames still flow through `direct_pkgnames` so
        // install_stratum marks them Explicit.
        assert_eq!(
            r.direct_pkgnames,
            vec![
                PkgName::from("otf-commit-mono"),
                PkgName::from("ttf-commit-mono"),
            ],
        );
    }

    #[test]
    fn pkgname_in_multi_pkgbase_restricts_to_that_pkgname() {
        // Regression: `gitaur -S bisq-cli` (a pkgname of the split pkgbase
        // `bisq`) used to install all three siblings because the by_name hit
        // was a bare passthrough — install_stratum has no way to filter
        // without a selection. Selection must pin the install to bisq-cli;
        // sibling pkgnames must NOT appear in direct_pkgnames.
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["bisq-cli"]), &mut select_all)
            .unwrap();
        assert_eq!(r.targets, vec!["bisq".to_owned()]);
        assert_eq!(r.direct_pkgnames, vec![PkgName::from("bisq-cli")]);
        assert_eq!(
            r.selections.get("bisq"),
            Some(&vec![PkgName::from("bisq-cli")]),
            "pkgname-target on split pkgbase must restrict install to that pkgname",
        );
    }

    #[test]
    fn pkgname_in_multi_pkgbase_pulls_sibling_runtime_deps() {
        // Per-pkgname runtime depends end up in the pkgbase-level
        // `e.depends` after .SRCINFO parsing — there's no per-pkgname
        // depends bucket today. If a sibling pkgname appears in that
        // pooled list, include it in the selection so pacman -U has the
        // intra-split dep on disk. Over-includes when the dep belongs to
        // a different sibling than the one targeted; that's a safe
        // over-install rather than a broken transaction.
        let idx = IndexFile {
            entries: vec![IndexEntry {
                pkgbase: "test-split".into(),
                pkgnames: vec![
                    Pkgname {
                        name: "test-split-core".into(),
                        provides: Vec::new(),
                        pkgdesc: None,
                    },
                    Pkgname {
                        name: "test-split-extras".into(),
                        provides: Vec::new(),
                        pkgdesc: None,
                    },
                ],
                depends: vec!["test-split-core".into()],
                pkgver: "1".into(),
                pkgrel: "1".into(),
                ..Default::default()
            }],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let pac = PacmanIndex::default();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &ts(&["test-split-extras"]),
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["test-split".to_owned()]);
        assert_eq!(
            r.direct_pkgnames,
            vec![PkgName::from("test-split-extras")],
            "direct_pkgnames stays at the user's explicit choice; sibling deps install --asdeps",
        );
        let mut sel = r.selections.get("test-split").cloned().unwrap_or_default();
        sel.sort();
        assert_eq!(
            sel,
            vec![
                PkgName::from("test-split-core"),
                PkgName::from("test-split-extras"),
            ],
            "sibling pkgname appearing in pkgbase.depends must join the install selection",
        );
    }

    #[test]
    fn pkgname_in_single_pkgbase_remains_passthrough() {
        // Trivial pkgbase (one pkgname): no selection needed, no
        // pkgbase-rewrite needed; the by_name passthrough is sufficient.
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["cower"]), &mut select_all)
            .unwrap();
        assert_eq!(r.targets, vec!["cower".to_owned()]);
        assert!(r.selections.is_empty());
        assert!(r.direct_pkgnames.is_empty());
    }

    #[test]
    fn multiple_pkgnames_in_same_split_pkgbase_merge_selection() {
        // `gitaur -S bisq-cli bisq-daemon` must install BOTH (and not
        // bisq-desktop). The second target must extend the existing
        // selection rather than overwrite it.
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &ts(&["bisq-cli", "bisq-daemon"]),
            &mut select_all,
        )
        .unwrap();
        assert_eq!(r.targets, vec!["bisq".to_owned(), "bisq".to_owned()]);
        let mut dp = r.direct_pkgnames.clone();
        dp.sort();
        assert_eq!(dp, vec!["bisq-cli".to_owned(), "bisq-daemon".to_owned()],);
        let mut sel = r.selections.get("bisq").cloned().unwrap_or_default();
        sel.sort();
        assert_eq!(
            sel,
            vec!["bisq-cli".to_owned(), "bisq-daemon".to_owned()],
            "multiple pkgname targets must accumulate into the same pkgbase selection",
        );
    }

    #[test]
    fn no_index_means_passthrough() {
        let (idx, _by, pac) = fixture();
        let r = expand_pkgbase_targets(&idx, None, &pac, &ts(&["bisq-single"]), &mut select_all)
            .unwrap();
        assert_eq!(r.targets, vec!["bisq-single".to_owned()]);
        assert!(r.selections.is_empty());
    }

    // ──────────────────────────────────────────────────────────────────
    // counterpart_hints — the dotnet-runtime fix at the expand layer.
    // ──────────────────────────────────────────────────────────────────

    /// Provides path: `-S bisq` rewrites to pkgbase `bisq`. The hint should
    /// be the virtual the user typed (`PkgName("bisq")`) — that's the
    /// installed name `counterpart_with_hint` will look for in localdb.
    #[test]
    fn provides_rewrite_records_virtual_as_hint() {
        let (idx, by, pac) = fixture();
        let r =
            expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["bisq"]), &mut select_all).unwrap();
        assert_eq!(
            r.counterpart_hints.get(&PkgBase::from("bisq")),
            Some(&PkgName::from("bisq")),
            "user typed `bisq` (a virtual) → hint must carry that pkgname",
        );
    }

    /// Pkgname-in-split path: `-S bisq-cli` rewrites to pkgbase `bisq`.
    /// Hint = the pkgname the user typed.
    #[test]
    fn split_pkgname_rewrite_records_pkgname_as_hint() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["bisq-cli"]), &mut select_all)
            .unwrap();
        assert_eq!(
            r.counterpart_hints.get(&PkgBase::from("bisq")),
            Some(&PkgName::from("bisq-cli")),
        );
    }

    /// Bare pkgbase path: `-S bisq-single` — the user typed the pkgbase
    /// itself, not a pkgname. No hint to record without an explicit one.
    #[test]
    fn bare_pkgbase_records_no_hint_when_none_supplied() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &ts(&["bisq-single"]),
            &mut select_all,
        )
        .unwrap();
        assert!(
            r.counterpart_hints.is_empty(),
            "no inferred hint for a pkgbase-typed target without explicit Target::hint",
        );
    }

    /// Explicit hint (the `-Syu` shape) overrides any inferred-from-spec
    /// hint even on the provides path. -Syu rows are
    /// `Target::with_hint(name, name)`, so the explicit hint matches the
    /// spec — but the precedence still must be exercised in case a future
    /// caller supplies a divergent hint.
    #[test]
    fn explicit_hint_overrides_inferred_hint() {
        let (idx, by, pac) = fixture();
        let explicit = vec![Target::with_hint("bisq", PkgName::from("bisq-cli"))];
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &explicit, &mut select_all).unwrap();
        assert_eq!(
            r.counterpart_hints.get(&PkgBase::from("bisq")),
            Some(&PkgName::from("bisq-cli")),
            "explicit Target::hint must win over the spec-derived hint",
        );
    }

    /// First hint wins when multiple targets rewrite to the same pkgbase
    /// (e.g. `-S bisq-cli bisq-daemon` both land on pkgbase `bisq`). The
    /// later target's hint must not clobber the first — counterpart only
    /// reads one hint per pkgbase and either would land on a valid sibling.
    #[test]
    fn first_target_hint_wins_on_collision() {
        let (idx, by, pac) = fixture();
        let r = expand_pkgbase_targets(
            &idx,
            Some(&by),
            &pac,
            &ts(&["bisq-cli", "bisq-daemon"]),
            &mut select_all,
        )
        .unwrap();
        assert_eq!(
            r.counterpart_hints.get(&PkgBase::from("bisq")),
            Some(&PkgName::from("bisq-cli")),
        );
    }

    /// Regression for the dotnet-runtime case as the user actually
    /// experienced it: the foreign virtual (`paru`) is *itself* installed
    /// (some prior AUR build registered that exact name in localdb), so
    /// `pac.is_installed(bare)` is true and the `record_target_hint` call
    /// at the top of the loop is the only thing that runs — the rewrite
    /// branches all short-circuit. Without that top-of-loop record, the
    /// hint would never reach `Plan.counterpart_hints` and `counterpart`
    /// would fall back to the first-declared provides (wrong).
    ///
    /// Verifies both that the spec passes through (resolver routes via
    /// `by_provides` itself) AND that the hint lands on the pkgbase.
    #[test]
    fn installed_foreign_virtual_records_hint_despite_pacman_shortcut() {
        let (idx, by, mut pac) = fixture();
        // `paru-bin` is the pkgbase; `paru` is its pkgbase-level provides.
        // Pretend `paru` itself is registered in localdb (foreign install
        // from some earlier source), mirroring the dotnet-runtime-7.0
        // shape — the name in `pac.installed` collides with the AUR
        // pkgbase's `provides`.
        pac.installed.insert("paru".into(), "1.0-1".into());
        let r =
            expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["paru"]), &mut select_all).unwrap();
        // Spec passed through unchanged — `pac.is_installed("paru")` is
        // true, so expand did NOT rewrite to the pkgbase string. Resolver
        // routes via `by_provides` in `resolve_target_source`.
        assert_eq!(r.targets, vec!["paru".to_owned()]);
        // The crucial bit: hint IS recorded despite the passthrough, so
        // `prepare_one` can pass it to `counterpart_with_hint`.
        assert_eq!(
            r.counterpart_hints.get(&PkgBase::from("paru-bin")),
            Some(&PkgName::from("paru")),
            "installed foreign virtual must record its hint even though \
             the spec passes through unchanged",
        );
    }

    /// Regression for the google-cloud-cli bug. `-Syu` rows for a
    /// foreign-installed pkgname in a split pkgbase hit the
    /// `pac.is_installed(bare)` shortcut. Twin to
    /// `installed_foreign_virtual_records_hint_despite_pacman_shortcut`,
    /// but for the **selection** side of the bookkeeping: without a
    /// recorded selection, `install_stratum` has no filter and `pacman
    /// -U`'s every sibling makepkg packaged. With the fix, the shortcut
    /// branch records the same single-pkgname (+ sibling runtime deps)
    /// selection that the `by_name` rewrite branch would have.
    #[test]
    fn installed_split_pkgname_records_selection_despite_pacman_shortcut() {
        let (idx, by, mut pac) = fixture();
        // bisq-cli is a pkgname of the split pkgbase `bisq` (siblings:
        // bisq-daemon, bisq-desktop; no intra-split deps in the fixture
        // → chosen reduces to just bisq-cli). Pretend it's installed
        // foreign at an outdated version, mirroring the
        // google-cloud-cli-bq starting state.
        pac.installed.insert("bisq-cli".into(), "1.0-1".into());
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["bisq-cli"]), &mut select_all)
            .unwrap();
        // Shortcut fired — spec passes through unchanged.
        assert_eq!(r.targets, vec!["bisq-cli".to_owned()]);
        // The crucial bit: selection IS recorded against the pkgbase.
        // Without this, install_stratum installs all three siblings.
        assert_eq!(
            r.selections.get(&PkgBase::from("bisq")),
            Some(&vec![PkgName::from("bisq-cli")]),
            "shortcut path must record the single-pkgname selection for \
             a foreign-installed sibling of a split pkgbase",
        );
    }

    /// Single-pkgname pkgbase under the shortcut: no selection needed
    /// (there's nothing to filter against). Guards against a refactor
    /// that records a degenerate `len==1` selection from the shortcut
    /// just because it can — `selections` is meaningful only when it
    /// constrains a *true subset*, and bloating it would muddy the
    /// `install_stratum` filter's "Some means subset" contract.
    #[test]
    fn installed_single_pkgname_records_no_selection_in_shortcut() {
        let (idx, by, mut pac) = fixture();
        // `cower` is a trivial single-pkgname pkgbase. Installed foreign.
        pac.installed.insert("cower".into(), "1.0-1".into());
        let r = expand_pkgbase_targets(&idx, Some(&by), &pac, &ts(&["cower"]), &mut select_all)
            .unwrap();
        assert_eq!(r.targets, vec!["cower".to_owned()]);
        assert!(
            r.selections.is_empty(),
            "single-pkgname pkgbase has no real subset to record",
        );
    }
}
