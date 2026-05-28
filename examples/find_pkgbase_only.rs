//! Find AUR entries where typing `-S <pkgbase>` would land on the
//! `by_pkgbase` fallback — i.e. the bare name doesn't match any pkgname or
//! provides anywhere in the index. Useful as a manual smoke test for the
//! pkgbase-only resolution path.
//!
//! Run with the index already populated:
//!
//!     cargo run --release --example find_pkgbase_only

use gitaur::index;
use gitaur::index::secondary::Secondary;
use gitaur::names::{PkgBase, PkgName};
use gitaur::paths;
use std::borrow::Borrow;

fn main() {
    let idx = index::load(&paths::index_path()).expect("load index — run `gaur -Sy` first");
    let by = Secondary::build(&idx);

    let mut hits: Vec<(PkgBase, Vec<PkgName>)> = Vec::new();
    for entry in &idx.entries {
        let pb = &entry.pkgbase;
        // Skip the trivial case where pkgbase already equals a pkgname —
        // those resolve via `by_name`, not the pkgbase fallback.
        if entry.pkgnames.iter().any(|p| pb.matches_pkgname(&p.name)) {
            continue;
        }
        // Skip when any pkgname's provides (or a pkgbase-level provides)
        // covers the bare pkgbase — those would short-circuit through
        // `provider_of` before reaching the pkgbase branch.
        if entry.all_provides().any(|x| pb == x) {
            continue;
        }
        // Cross-domain probe: does the pkgbase string also appear as a
        // pkgname or virtual-provides key? `Borrow<str>` is the one cast
        // the typed wrappers expose for HashMap-key interop.
        let pb_str: &str = pb.borrow();
        if by.by_name.contains_key(pb_str) || by.by_provides.contains_key(pb_str) {
            continue;
        }
        hits.push((
            pb.clone(),
            entry.pkgnames.iter().map(|p| p.name.clone()).collect(),
        ));
    }

    // Sort by pkgname-count ascending (single-pkgname pkgbases build faster
    // and are easier to reason about), then alphabetically.
    hits.sort_by(|a, b| a.1.len().cmp(&b.1.len()).then_with(|| a.0.cmp(&b.0)));

    println!("Found {} pkgbase-only candidates", hits.len());

    // Now narrow to "fast-to-build" candidates: no depends, no makedepends,
    // no checkdepends. Multi-pkgname pkgbases trigger the MultiSelect prompt
    // in `ui::select_pkgnames`, which is the path we want to exercise.
    println!();
    println!("Split pkgbases (multiple pkgnames) — triggers the MultiSelect prompt:");
    println!();
    let mut count = 0;
    for (pkgbase, pkgnames) in &hits {
        if pkgnames.len() < 2 {
            continue;
        }
        let entry = idx.entries.iter().find(|e| &e.pkgbase == pkgbase).unwrap();
        if !entry.depends.is_empty()
            || !entry.makedepends.is_empty()
            || !entry.checkdepends.is_empty()
        {
            continue;
        }
        let desc = entry.pkgdesc.as_deref().unwrap_or("");
        // `Vec<PkgName>::join` resolves through `Borrow<str>` — the same
        // mechanical impl that backs `report.installed.join(" ")` over
        // `Vec<PkgBase>` in build/print.rs. No per-element cast needed.
        println!(
            "  {pkgbase}  ({} pkgnames)  →  [{}]  {desc}",
            pkgnames.len(),
            pkgnames.join(", "),
        );
        count += 1;
        if count >= 30 {
            break;
        }
    }
}
