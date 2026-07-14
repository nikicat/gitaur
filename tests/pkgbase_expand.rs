//! Integration: end-to-end exercise of `expand_pkgbase_targets` against the
//! same primary→secondary index pipeline the real CLI runs. Locks in:
//!
//! * `-S <pkgbase>` hits the `by_pkgbase` fallback and emits the **pkgbase**
//!   as the resolver target (not the pkgnames) so `by_name` collisions
//!   with unrelated pkgbases can't substitute the wrong entry;
//! * the chosen pkgnames flow through `direct_pkgnames` so the caller can
//!   merge them into `Plan.direct_targets` for Explicit marking;
//! * partial-selection on a split pkgbase records the install-filter
//!   constraint;
//! * provides-only references (`bisq` → `bisq-desktop`) rewrite via
//!   `provider_of` and record the matching install constraint;
//! * pkgname inputs still pass through verbatim and skip the selector;
//! * the `by_name` collision case (commit-mono-font shape) doesn't land an
//!   unrelated pkgbase in the build plan.

use aurox::build::Target;
use aurox::config::defaults::default_config;
use aurox::error::Result;
use aurox::index::build::full_build;
use aurox::index::lookup::Lookup;
use aurox::mirror::MirrorRepo;
use aurox::names::{PkgBase, PkgName, PkgTargetSetExt};
use aurox::pacman::alpm_db::PacmanIndex;
use aurox::resolver::{expand_pkgbase_targets, resolve};
use aurox::testing::git;
use std::path::Path;
use tempfile::TempDir;

/// Wrap bare specs as hint-less `Target`s — the integration tests exercise
/// the `-S` argv shape where `expand_pkgbase_targets` is expected to
/// derive any hint from the spec itself.
fn ts(specs: &[&str]) -> Vec<Target> {
    specs.iter().copied().map(Target::bare).collect()
}

/// Build a bare repo with one branch per (pkgbase, .SRCINFO content) tuple.
fn build_mirror(root: &Path, branches: &[(&str, &str)]) -> std::path::PathBuf {
    let src = root.join("src");
    let bare = root.join("bare");
    std::fs::create_dir_all(&src).unwrap();
    git(&["init", "-q", "-b", "trunk"], &src);
    for (i, (branch, srcinfo)) in branches.iter().enumerate() {
        git(&["checkout", "-q", "-b", branch], &src);
        std::fs::write(src.join(".SRCINFO"), srcinfo).unwrap();
        git(&["add", "-A"], &src);
        git(&["commit", "-q", "-m", &format!("c{i}")], &src);
    }
    git(
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
        root,
    );
    bare
}

#[test]
fn pkgbase_only_target_expands_with_pkgbase_target_and_direct_pkgnames() {
    let dir = TempDir::new().unwrap();
    let bare = build_mirror(
        dir.path(),
        &[(
            "bisq",
            "pkgbase = bisq\npkgver = 1\npkgrel = 1\npkgname = bisq-desktop\n",
        )],
    );

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let idx = full_build(&cfg, &mirror).unwrap();
    let by = Lookup::build(&idx);
    let pac = PacmanIndex::default();

    let mut select_called = false;
    let mut select = |pkgbase: &PkgBase, pkgnames: &[PkgName]| -> Result<Vec<PkgName>> {
        select_called = true;
        assert_eq!(pkgbase, &PkgBase::from("bisq"));
        assert_eq!(pkgnames, &[PkgName::from("bisq-desktop")]);
        Ok(pkgnames.to_vec())
    };
    let expanded = expand_pkgbase_targets(&idx, &by, &pac, &ts(&["bisq"]), &mut select).unwrap();
    assert!(
        select_called,
        "selector must run even for single-pkgname pkgbase so callers can log/notice it",
    );
    assert_eq!(
        expanded.targets,
        vec!["bisq".to_owned()],
        "resolver target is the pkgbase (avoids by_name aliasing)",
    );
    assert_eq!(
        expanded.direct_pkgnames,
        vec![PkgName::from("bisq-desktop")],
        "the chosen pkgname is the user's actual direct target",
    );
    assert!(
        expanded.selections.is_empty(),
        "full selection (1/1) is the default — no install filter constraint",
    );

    // End-to-end: resolver accepts the pkgbase, plan has one stratum, and
    // the caller-side direct_pkgnames merge lets install_stratum recognise
    // bisq-desktop as Explicit.
    let mut plan = resolve(&cfg, &idx, &by, &pac, &expanded.targets).unwrap();
    plan.direct_targets
        .extend(expanded.direct_pkgnames.into_iter().map(Into::into));
    assert_eq!(plan.aur_strata, vec![vec![PkgBase::from("bisq")]]);
    assert!(
        plan.direct_targets
            .contains_pkgname(&PkgName::from("bisq-desktop"))
    );
}

#[test]
fn split_pkgbase_partial_selection_constrains_build_pipeline() {
    let dir = TempDir::new().unwrap();
    let bare = build_mirror(
        dir.path(),
        &[(
            "linux-headers-multi",
            "pkgbase = linux-headers-multi\n\
             pkgver = 6.7\npkgrel = 1\n\
             pkgname = linux-headers-multi-core\n\
             pkgname = linux-headers-multi-extras\n\
             pkgname = linux-headers-multi-docs\n",
        )],
    );

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let idx = full_build(&cfg, &mirror).unwrap();
    let by = Lookup::build(&idx);
    let pac = PacmanIndex::default();

    // User picks only two of the three split pkgnames.
    let mut select = |pkgbase: &PkgBase, pkgnames: &[PkgName]| -> Result<Vec<PkgName>> {
        assert_eq!(pkgbase, &PkgBase::from("linux-headers-multi"));
        assert_eq!(pkgnames.len(), 3);
        Ok(vec![
            PkgName::from("linux-headers-multi-core"),
            PkgName::from("linux-headers-multi-extras"),
        ])
    };
    let expanded =
        expand_pkgbase_targets(&idx, &by, &pac, &ts(&["linux-headers-multi"]), &mut select)
            .unwrap();

    assert_eq!(
        expanded.targets,
        vec!["linux-headers-multi".to_owned()],
        "pkgbase string is what the resolver sees",
    );
    assert_eq!(
        expanded.direct_pkgnames,
        vec![
            PkgName::from("linux-headers-multi-core"),
            PkgName::from("linux-headers-multi-extras"),
        ],
    );
    assert_eq!(
        expanded
            .selections
            .get(&PkgBase::from("linux-headers-multi")),
        Some(&vec![
            PkgName::from("linux-headers-multi-core"),
            PkgName::from("linux-headers-multi-extras"),
        ]),
        "partial selection must be recorded so the install filter can apply it",
    );

    let mut plan = resolve(&cfg, &idx, &by, &pac, &expanded.targets).unwrap();
    plan.pkgname_selections = expanded.selections;
    plan.direct_targets
        .extend(expanded.direct_pkgnames.into_iter().map(Into::into));
    assert_eq!(
        plan.aur_strata,
        vec![vec![PkgBase::from("linux-headers-multi")]]
    );
    assert!(
        plan.direct_targets
            .contains_pkgname(&PkgName::from("linux-headers-multi-core"))
    );
    assert!(
        plan.direct_targets
            .contains_pkgname(&PkgName::from("linux-headers-multi-extras"))
    );
    assert!(
        !plan
            .direct_targets
            .contains_pkgname(&PkgName::from("linux-headers-multi-docs"))
    );
    assert_eq!(plan.pkgname_selections.len(), 1);
}

/// The bisq regression locked in end-to-end: a 3-way split pkgbase where one
/// pkgname declares `provides = <virtual>`. Typing the virtual name resolves
/// via `provider_of`, the resolver receives the pkgbase string, and the
/// chosen pkgname is recorded both as a direct target and as the
/// install-filter constraint so the siblings don't end up installed.
#[test]
fn provides_target_rewrites_to_providing_pkgname_with_selection() {
    let dir = TempDir::new().unwrap();
    let bare = build_mirror(
        dir.path(),
        &[(
            "bisq",
            "pkgbase = bisq\n\
             pkgver = 1.9.22\npkgrel = 2\n\
             pkgname = bisq-desktop\n\
                provides = bisq\n\
             pkgname = bisq-cli\n\
             pkgname = bisq-daemon\n",
        )],
    );

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let idx = full_build(&cfg, &mirror).unwrap();
    let by = Lookup::build(&idx);
    let pac = PacmanIndex::default();

    let mut selector_invoked = false;
    let mut select = |_p: &PkgBase, _n: &[PkgName]| -> Result<Vec<PkgName>> {
        selector_invoked = true;
        Ok(vec![])
    };
    let expanded = expand_pkgbase_targets(&idx, &by, &pac, &ts(&["bisq"]), &mut select).unwrap();

    assert!(
        !selector_invoked,
        "provides hit must rewrite via provider_of, not delegate to the selector",
    );
    assert_eq!(
        expanded.targets,
        vec!["bisq".to_owned()],
        "resolver target is the pkgbase; `by_pkgbase` pins to the right entry",
    );
    assert_eq!(
        expanded.direct_pkgnames,
        vec![PkgName::from("bisq-desktop")],
        "the providing pkgname is the user's actual direct target",
    );
    assert_eq!(
        expanded.selections.get(&PkgBase::from("bisq")),
        Some(&vec![PkgName::from("bisq-desktop")]),
        "scoped provides records a one-pkgname install-filter constraint",
    );

    let mut plan = resolve(&cfg, &idx, &by, &pac, &expanded.targets).unwrap();
    plan.pkgname_selections = expanded.selections;
    plan.direct_targets
        .extend(expanded.direct_pkgnames.into_iter().map(Into::into));
    assert_eq!(plan.aur_strata, vec![vec![PkgBase::from("bisq")]]);
    assert!(
        plan.direct_targets
            .contains_pkgname(&PkgName::from("bisq-desktop"))
    );
    assert!(
        !plan
            .direct_targets
            .contains_pkgname(&PkgName::from("bisq-cli"))
    );
    assert!(
        !plan
            .direct_targets
            .contains_pkgname(&PkgName::from("bisq-daemon"))
    );
    assert_eq!(
        plan.pkgname_selections
            .get(&PkgBase::from("bisq"))
            .map(Vec::len),
        Some(1)
    );
}

/// Regression for the commit-mono-font case. AUR has both:
///   * pkgbase `commit-mono-font` producing `otf-commit-mono` + `ttf-commit-mono`,
///   * a separate pkgbase `otf-commit-mono` whose sole pkgname is `otf-commit-mono`.
///
/// `by_name["otf-commit-mono"]` only stores one entry (`HashMap` insert-order
/// winner). If `expand_pkgbase_targets` handed pkgnames to the resolver, the
/// resolver would silently classify `otf-commit-mono` into the *unrelated*
/// pkgbase and end up with a plan that builds two pkgbases. Passing the
/// pkgbase string through `by_pkgbase` (which is unique) keeps the plan
/// scoped to a single entry.
#[test]
fn pkgname_collision_with_another_pkgbase_does_not_leak_into_plan() {
    let dir = TempDir::new().unwrap();
    // Order matters: alphabetically `commit-mono-font` < `otf-commit-mono`,
    // so the standalone pkgbase wins the HashMap insert race in
    // Lookup::build — same alignment as the real AUR mirror.
    let bare = build_mirror(
        dir.path(),
        &[
            (
                "commit-mono-font",
                "pkgbase = commit-mono-font\n\
                 pkgver = 1.143\npkgrel = 2\n\
                 pkgname = otf-commit-mono\n\
                 pkgname = ttf-commit-mono\n",
            ),
            (
                "otf-commit-mono",
                "pkgbase = otf-commit-mono\n\
                 pkgver = 1.142\npkgrel = 1\n\
                 pkgname = otf-commit-mono\n",
            ),
        ],
    );

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let idx = full_build(&cfg, &mirror).unwrap();
    let by = Lookup::build(&idx);
    let pac = PacmanIndex::default();

    let mut select = |_pb: &PkgBase, pns: &[PkgName]| -> Result<Vec<PkgName>> { Ok(pns.to_vec()) };
    let expanded =
        expand_pkgbase_targets(&idx, &by, &pac, &ts(&["commit-mono-font"]), &mut select).unwrap();

    assert_eq!(
        expanded.targets,
        vec!["commit-mono-font".to_owned()],
        "must pass the pkgbase string; pkgnames would alias to the wrong entry via by_name",
    );
    assert_eq!(
        expanded.direct_pkgnames,
        vec![
            PkgName::from("otf-commit-mono"),
            PkgName::from("ttf-commit-mono"),
        ],
    );

    let mut plan = resolve(&cfg, &idx, &by, &pac, &expanded.targets).unwrap();
    plan.direct_targets
        .extend(expanded.direct_pkgnames.into_iter().map(Into::into));

    // The crucial assertion: only one pkgbase in the plan, and it's the
    // right one. Without the fix, `aur_strata` would have *two* entries:
    // commit-mono-font AND otf-commit-mono.
    assert_eq!(
        plan.aur_strata,
        vec![vec![PkgBase::from("commit-mono-font")]],
        "the unrelated otf-commit-mono pkgbase must NOT leak into the build plan",
    );
    assert!(
        plan.direct_targets
            .contains_pkgname(&PkgName::from("otf-commit-mono"))
    );
    assert!(
        plan.direct_targets
            .contains_pkgname(&PkgName::from("ttf-commit-mono"))
    );
}

#[test]
fn pkgname_target_skips_selector_even_when_pkgbase_could_match() {
    // pkgbase `cower` *also* has pkgname `cower` — by_name wins, selector
    // never fires. Catches a regression where pkgbase fallback accidentally
    // took precedence.
    let dir = TempDir::new().unwrap();
    let bare = build_mirror(
        dir.path(),
        &[(
            "cower",
            "pkgbase = cower\npkgver = 17\npkgrel = 2\npkgname = cower\n",
        )],
    );

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let idx = full_build(&cfg, &mirror).unwrap();
    let by = Lookup::build(&idx);
    let pac = PacmanIndex::default();

    let mut calls = 0;
    let mut select = |_p: &PkgBase, n: &[PkgName]| -> Result<Vec<PkgName>> {
        calls += 1;
        Ok(n.to_vec())
    };
    let expanded = expand_pkgbase_targets(&idx, &by, &pac, &ts(&["cower"]), &mut select).unwrap();
    assert_eq!(expanded.targets, vec!["cower".to_owned()]);
    assert_eq!(calls, 0, "selector must not run on pkgname hits");
}
