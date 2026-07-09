//! Find AUR pkgbases that resolve to the deepest stratum stack — i.e. the
//! biggest build pipeline `aurox -S <pkg>` would print (decline the
//! confirmation prompt for a dry run).
//!
//! Uses the real resolver, so the strata count matches what aurox would
//! actually do. Skips entries the resolver rejects (cycles, missing deps).

use aurox::config::defaults::default_config;
use aurox::index::{self, secondary::Secondary};
use aurox::names::PkgBase;
use aurox::pacman::alpm_db::{self, PacmanIndex};
use aurox::paths;
use aurox::resolver;

fn main() {
    let idx = index::load(&paths::index_path()).expect("load index — run `aurox -Sy` first");
    let by = Secondary::build(&idx);
    let alpm = alpm_db::open().expect("open alpm");
    let pac = PacmanIndex::build(&alpm);
    let cfg = default_config();

    let mut ranked: Vec<(PkgBase, usize, usize)> = Vec::new();
    let total = idx.entries.len();
    for (i, entry) in idx.entries.iter().enumerate() {
        if i % 5000 == 0 {
            eprintln!("scanning {i}/{total}");
        }
        // python38-* form a giant cyclical cluster. Dedicated method on
        // PkgBase rather than reaching into the inner string.
        if entry.pkgbase.starts_with("python38-") {
            continue;
        }
        // Pick the first pkgname as the user-typed target.
        let Some(pkgname) = entry.pkgnames.first() else {
            continue;
        };
        let targets = vec![pkgname.name.clone().into_inner()];
        let Ok(plan) = resolver::resolve(&cfg, &idx, Some(&by), &pac, &targets) else {
            continue;
        };
        let strata = plan.aur_strata.len();
        let aur_total = plan.aur_strata.iter().map(Vec::len).sum::<usize>();
        if strata >= 3 {
            ranked.push((entry.pkgbase.clone(), strata, aur_total));
        }
    }
    // Sort by strata DESC, then by AUR pkg count ASC (smaller, comprehensible).
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));

    println!("\nTop 30 pkgbases by resolved stratum count:");
    for (pb, strata, total) in ranked.iter().take(30) {
        let entry = idx.entries.iter().find(|e| e.pkgbase == *pb).unwrap();
        let desc = entry.pkgdesc.as_deref().unwrap_or("");
        println!("  strata={strata} aur_pkgs={total}  {pb}  — {desc}");
    }
}
