//! The `-Si` / shell-`info` block for AUR packages.
//!
//! Most fields come straight from the [`IndexEntry`]; three live outside the
//! index file and are resolved per target by [`InfoSources`]:
//!
//! * **Maintainer** — the `# Maintainer:` comment convention in the PKGBUILD
//!   blob at the entry's indexed tip commit (the AUR RPC's Maintainer field
//!   has no git-side equivalent, but the comment carries the same
//!   information).
//! * **First Submitted** — the committer time of the branch's root commit.
//! * **Installed Size** — localdb `isize()` for members already installed
//!   (an AUR pkgbase has no syncdb to quote a size from before it's built).

use crate::config::Config;
use crate::error::Result;
use crate::index::schema::{IndexEntry, IndexFile};
use crate::index::{AurState, load_or_resync, secondary};
use crate::names::{Maintainer, PkgName, PkgTarget};
use crate::pacman::alpm_db;
use crate::paths;
use crate::ui;
use crate::units::{ByteSize, UnixTime};
use alpm::Alpm;
use gix::ObjectId;
use std::fmt::Display;
use std::io::{self, Write};
use tracing::debug;

/// `-Si` info for one or more targets (AUR-only by design — repo packages are
/// `pacman -Si`'s job on this path; the interactive shell merges the two).
pub fn cmd_info(cfg: &Config, targets: &[PkgTarget]) -> Result<u8> {
    // Without this guard a missing index loads as *empty* and every target
    // reads "not in AUR" — wrong diagnosis, the AUR just isn't in play yet
    // (or is off by choice).
    match AurState::probe(cfg) {
        AurState::Ready => {}
        AurState::NotSetUp => {
            ui::warn("no AUR index; run `aurox -Sy` first");
            return Ok(1);
        }
        AurState::Disabled => {
            ui::warn("AUR info is disabled (aur = false in config.toml)");
            return Ok(1);
        }
    }
    let idx = load_or_resync(cfg, &paths::index_path())?;
    let by = secondary::Secondary::build(&idx);
    let sources = InfoSources::open();
    let missing: Vec<&PkgTarget> = targets
        .iter()
        .filter(|t| !print_aur_info(&idx, &by, t, &sources))
        .collect();
    if !missing.is_empty() {
        ui::warn(&format!(
            "not in AUR: {}",
            missing
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // Pacman-style exit code: non-zero when a requested target wasn't in the AUR.
    Ok(u8::from(!missing.is_empty()))
}

/// Look up one target (pkgname / provides / pkgbase, via [`secondary`]) against
/// an already-loaded index and print its `-Si`-style block. `false` ⇒ not in
/// the AUR — the caller decides how to report the miss ([`cmd_info`] warns
/// "not in AUR"; the shell first tries the sync repos and words it
/// accordingly). Shared so both surfaces resolve a name identically.
pub(crate) fn print_aur_info(
    idx: &IndexFile,
    by: &secondary::Secondary,
    target: &PkgTarget,
    sources: &InfoSources,
) -> bool {
    match by.lookup(idx, target.as_str()) {
        Some(entry) => {
            print_info(entry, &sources.extras(entry));
            true
        }
        None => false,
    }
}

/// Live handles behind the block's out-of-index fields.
///
/// The bare AUR mirror (maintainer comment, first-submitted) and the local
/// pacman DB (installed size). Opened best-effort once per `info` command —
/// a source that fails to open just leaves its fields off every block,
/// never fails the command.
pub struct InfoSources {
    repo: Option<gix::Repository>,
    alpm: Option<Alpm>,
}

impl InfoSources {
    pub fn open() -> Self {
        let repo = gix::open(paths::aur_repo_path())
            .inspect_err(|e| debug!(error = %e, "mirror unavailable for info extras"))
            .ok();
        let alpm = alpm_db::open()
            .inspect_err(|e| debug!(error = %e, "alpm unavailable for info extras"))
            .ok();
        Self { repo, alpm }
    }

    /// Resolve the extras for one entry. Each lookup is independently
    /// best-effort: a branch whose history can't be walked still gets its
    /// installed size, and vice versa.
    fn extras(&self, e: &IndexEntry) -> Extras {
        let mut x = Extras::default();
        let tip = ObjectId::from(e.commit_oid);
        if let Some(repo) = &self.repo
            && !tip.is_null()
        {
            x.maintainers = maintainers_at(repo, tip).unwrap_or_default();
            x.first_submitted = first_submitted(repo, tip);
        }
        if let Some(alpm) = &self.alpm {
            for p in &e.pkgnames {
                if let Ok(pkg) = alpm.localdb().pkg(p.name.as_str()) {
                    x.installed.push(InstalledMember {
                        name: p.name.clone(),
                        size: ByteSize::new(u64::try_from(pkg.isize()).unwrap_or(0)),
                    });
                }
            }
        }
        x
    }
}

/// The out-of-index half of one entry's block (see [`InfoSources`]). Absent
/// fields simply omit their lines — [`Extras::default`] renders the same
/// block the index alone can produce.
#[derive(Default)]
struct Extras {
    maintainers: Vec<Maintainer>,
    first_submitted: Option<UnixTime>,
    /// Already-installed members, in `pkgnames` order.
    installed: Vec<InstalledMember>,
}

/// One already-installed member of the pkgbase and its localdb `isize`.
struct InstalledMember {
    name: PkgName,
    size: ByteSize,
}

/// Print the block to stdout (the interactive path). Same best-effort stance
/// as the `println!`-based printers elsewhere: a closed stdout mid-block
/// isn't worth failing the command over.
fn print_info(e: &IndexEntry, x: &Extras) {
    let stdout = io::stdout();
    write_info(&mut stdout.lock(), e, x).ok();
}

/// Render the block to `out` in pacman's `-Si` field order (aurox-specific
/// fields slot in next to their nearest pacman analogue). Empty fields are
/// omitted, not rendered as `None` — the long-standing aurox stance. A
/// writer (not `println!`) so the exact byte layout is testable without
/// capturing a process's stdout.
fn write_info<W: Write>(out: &mut W, e: &IndexEntry, x: &Extras) -> io::Result<()> {
    field(out, Label::Repository, "aur")?;
    field(out, Label::Name, &e.pkgbase)?;
    // Show the split-pkg list whenever the entry actually has more than one
    // pkgname (or the single pkgname differs from pkgbase). Members carrying
    // their own pkgdesc render as `name: desc`, so split-package descriptions
    // surface without a per-member block.
    let trivial = e.pkgnames.len() == 1 && e.pkgbase.matches_pkgname(&e.pkgnames[0].name);
    if !e.pkgnames.is_empty() && !trivial {
        let members: Vec<String> = e
            .pkgnames
            .iter()
            .map(|p| match p.pkgdesc.as_deref() {
                Some(d) if !d.is_empty() => format!("{}: {d}", p.name),
                _ => p.name.to_string(),
            })
            .collect();
        multiline_field(out, Label::SplitPkgs, &members)?;
    }
    field(out, Label::Version, e.version())?;
    if let Some(d) = e.display_desc() {
        field(out, Label::Description, d)?;
    }
    list_field(out, Label::Architecture, &e.arch)?;
    if let Some(u) = &e.url {
        field(out, Label::Url, u)?;
    }
    // Union of pkgbase-level and pkgname-scoped provides — `-Si` users
    // want to see every virtual name the pkgbase makes available, not the
    // attribution.
    let provides: Vec<&PkgTarget> = e.all_provides().collect();
    list_field(out, Label::Provides, &provides)?;
    list_field(out, Label::DependsOn, &e.depends)?;
    list_field(out, Label::MakeDeps, &e.makedepends)?;
    list_field(out, Label::CheckDeps, &e.checkdepends)?;
    // One optdep per line, pacman-style — the `: reason` halves would blur
    // together space-joined.
    let optdeps: Vec<String> = e.optdepends.iter().map(ToString::to_string).collect();
    multiline_field(out, Label::OptionalDeps, &optdeps)?;
    list_field(out, Label::ConflictsWith, &e.conflicts)?;
    list_field(out, Label::Replaces, &e.replaces)?;
    // localdb sizes exist only for installed members. Split packages label
    // each member's line; the trivial single-pkgname case is just the size.
    let sizes: Vec<String> = x
        .installed
        .iter()
        .map(|m| {
            if trivial {
                m.size.to_string()
            } else {
                format!("{}: {}", m.name, m.size)
            }
        })
        .collect();
    multiline_field(out, Label::InstalledSize, &sizes)?;
    let maintainers: Vec<String> = x.maintainers.iter().map(ToString::to_string).collect();
    multiline_field(out, Label::Maintainer, &maintainers)?;
    if let Some(t) = x.first_submitted.and_then(UnixTime::render) {
        field(out, Label::FirstSubmitted, t)?;
    }
    if let Some(t) = e.commit_time.render() {
        field(out, Label::LastUpdated, t)?;
    }
    writeln!(out)
}

/// A field label of the info block — the closed vocabulary both blocks
/// (AUR here, repo in [`crate::pacman::alpm_db::SyncInfo`]) draw from, so
/// they can't drift into near-miss labels ("Depends" vs "Depends On") and
/// a typo'd free string can't compile. Every label must fit the 16-column
/// gutter [`field`] aligns on; a unit test pins that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Label {
    Repository,
    Name,
    SplitPkgs,
    Version,
    Description,
    Architecture,
    Url,
    Provides,
    DependsOn,
    MakeDeps,
    CheckDeps,
    OptionalDeps,
    ConflictsWith,
    Replaces,
    DownloadSize,
    InstalledSize,
    Maintainer,
    Packager,
    FirstSubmitted,
    LastUpdated,
    BuildDate,
}

impl Label {
    /// Every variant, for the gutter-width test.
    #[cfg(test)]
    const ALL: [Self; 21] = [
        Self::Repository,
        Self::Name,
        Self::SplitPkgs,
        Self::Version,
        Self::Description,
        Self::Architecture,
        Self::Url,
        Self::Provides,
        Self::DependsOn,
        Self::MakeDeps,
        Self::CheckDeps,
        Self::OptionalDeps,
        Self::ConflictsWith,
        Self::Replaces,
        Self::DownloadSize,
        Self::InstalledSize,
        Self::Maintainer,
        Self::Packager,
        Self::FirstSubmitted,
        Self::LastUpdated,
        Self::BuildDate,
    ];

    const fn text(self) -> &'static str {
        match self {
            Self::Repository => "Repository",
            Self::Name => "Name",
            Self::SplitPkgs => "Split pkgs",
            Self::Version => "Version",
            Self::Description => "Description",
            Self::Architecture => "Architecture",
            Self::Url => "URL",
            Self::Provides => "Provides",
            Self::DependsOn => "Depends On",
            Self::MakeDeps => "Make Deps",
            Self::CheckDeps => "Check Deps",
            Self::OptionalDeps => "Optional Deps",
            Self::ConflictsWith => "Conflicts With",
            Self::Replaces => "Replaces",
            Self::DownloadSize => "Download Size",
            Self::InstalledSize => "Installed Size",
            Self::Maintainer => "Maintainer",
            Self::Packager => "Packager",
            Self::FirstSubmitted => "First Submitted",
            Self::LastUpdated => "Last Updated",
            Self::BuildDate => "Build Date",
        }
    }
}

impl Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.text())
    }
}

/// One field line: 16-char label + `: ` + value — pacman's `-Si` alignment.
/// `pub(crate)` with its two list siblings so the repo block
/// ([`crate::pacman::alpm_db::SyncInfo`]) renders through the same layout
/// and the two info blocks can't drift apart.
pub(crate) fn field<W: Write>(out: &mut W, label: Label, value: impl Display) -> io::Result<()> {
    writeln!(out, "{label:<16}: {value}")
}

/// Space-joined list field, omitted when empty.
pub(crate) fn list_field<W: Write>(
    out: &mut W,
    label: Label,
    items: &[impl Display],
) -> io::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let joined = items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    field(out, label, joined)
}

/// One value per line, continuation lines indented to the value column —
/// how pacman renders `Optional Deps`. Omitted when empty.
pub(crate) fn multiline_field<W: Write>(
    out: &mut W,
    label: Label,
    values: &[String],
) -> io::Result<()> {
    for (i, v) in values.iter().enumerate() {
        if i == 0 {
            field(out, label, v)?;
        } else {
            writeln!(out, "{:<18}{v}", "")?;
        }
    }
    Ok(())
}

/// `# Maintainer:` comment values from a PKGBUILD, in file order.
///
/// An AUR convention, not machine-enforced: the current maintainer(s) head
/// the file as `# Maintainer: Name <email>`, previous ones demoted to
/// `# Contributor:`. Whole-file scan since nothing guarantees the header
/// block comes first; only comment lines are considered.
fn maintainers(pkgbuild: &str) -> Vec<Maintainer> {
    pkgbuild
        .lines()
        .filter_map(|line| {
            let comment = line.trim_start().strip_prefix('#')?;
            let (key, value) = comment.split_once(':')?;
            let key = key.trim();
            (key.eq_ignore_ascii_case("maintainer") || key.eq_ignore_ascii_case("maintainers"))
                .then(|| value.trim())
                .filter(|v| !v.is_empty())
                .map(Maintainer::new)
        })
        .collect()
}

/// [`maintainers`] over the PKGBUILD blob at the entry's indexed tip commit.
/// `None` on any lookup failure — the block just omits the field.
fn maintainers_at(repo: &gix::Repository, tip: ObjectId) -> Option<Vec<Maintainer>> {
    let tree = repo.find_commit(tip).ok()?.tree().ok()?;
    let entry = tree.find_entry("PKGBUILD")?;
    let blob = repo.find_object(entry.oid().to_owned()).ok()?;
    Some(maintainers(&String::from_utf8_lossy(blob.data.as_slice())))
}

/// Committer time of the branch's root commit — when the pkgbase first
/// appeared on the AUR. Walks the whole branch history; AUR package
/// histories are short (typically tens of commits), so per-target cost is
/// negligible. Multiple roots (a history graft) take the earliest. `None`
/// on any walk hiccup — the block just omits the field.
fn first_submitted(repo: &gix::Repository, tip: ObjectId) -> Option<UnixTime> {
    let walk = repo.find_commit(tip).ok()?.ancestors().all().ok()?;
    let mut oldest: Option<i64> = None;
    for info in walk {
        let info = info.ok()?;
        if info.parent_ids.is_empty() {
            let t = info.object().ok()?.time().ok()?.seconds;
            oldest = Some(oldest.map_or(t, |o: i64| o.min(t)));
        }
    }
    oldest.map(UnixTime::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::Pkgname;
    use crate::names::{Arch, Url};
    use crate::{assert_contains, assert_not_contains};

    fn render(e: &IndexEntry, x: &Extras) -> String {
        let mut buf: Vec<u8> = Vec::new();
        write_info(&mut buf, e, x).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn mk(pkgbase: &str) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: vec![Pkgname {
                name: pkgbase.into(),
                provides: Vec::new(),
                pkgdesc: None,
            }],
            pkgver: "1.0".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    fn member(name: &str, desc: Option<&str>) -> Pkgname {
        Pkgname {
            name: name.into(),
            provides: Vec::new(),
            pkgdesc: desc.map(str::to_owned),
        }
    }

    #[test]
    fn minimal_entry_renders_header_fields_only() {
        let out = render(&mk("foo"), &Extras::default());
        assert_eq!(
            out,
            "Repository      : aur\n\
             Name            : foo\n\
             Version         : 1.0-1\n\
             \n"
        );
    }

    #[test]
    fn full_entry_renders_pacman_si_field_order() {
        let mut e = mk("foo");
        e.pkgdesc = Some("does foo".into());
        e.url = Some(Url::new("https://foo.example"));
        e.arch = vec![Arch::new("i686"), Arch::new("x86_64")];
        e.provides = vec![PkgTarget::new("libfoo.so")];
        e.depends = vec![PkgTarget::new("glibc>=2.38")];
        e.makedepends = vec![PkgTarget::new("cmake")];
        e.checkdepends = vec![PkgTarget::new("python-pytest")];
        e.optdepends = vec!["cups: printing support".into(), "bash-completion".into()];
        e.conflicts = vec![PkgTarget::new("foo-git")];
        e.replaces = vec![PkgTarget::new("foo-legacy")];
        let out = render(&e, &Extras::default());
        assert_eq!(
            out,
            "Repository      : aur\n\
             Name            : foo\n\
             Version         : 1.0-1\n\
             Description     : does foo\n\
             Architecture    : i686 x86_64\n\
             URL             : https://foo.example\n\
             Provides        : libfoo.so\n\
             Depends On      : glibc>=2.38\n\
             Make Deps       : cmake\n\
             Check Deps      : python-pytest\n\
             Optional Deps   : cups: printing support\n                  bash-completion\n\
             Conflicts With  : foo-git\n\
             Replaces        : foo-legacy\n\
             \n"
        );
    }

    #[test]
    fn extras_render_after_the_index_fields() {
        let mut e = mk("foo");
        e.commit_time = UnixTime::new(1_700_000_000);
        let x = Extras {
            maintainers: vec![Maintainer::new("Jane Doe <jane@example.org>")],
            first_submitted: Some(UnixTime::new(1_600_000_000)),
            installed: vec![InstalledMember {
                name: PkgName::new("foo"),
                size: ByteSize::new(12 * 1024 * 1024),
            }],
        };
        let out = render(&e, &x);
        assert_contains!(out, "Installed Size  : 12.00 MiB\n");
        assert_contains!(out, "Maintainer      : Jane Doe <jane@example.org>\n");
        // System-timezone rendering makes the exact text environment-dependent;
        // presence and ordering are what this pins.
        let submitted = out.find("First Submitted").unwrap();
        let updated = out.find("Last Updated").unwrap();
        assert!(submitted < updated, "field order regressed:\n{out}");
    }

    #[test]
    fn unknown_commit_time_omits_last_updated() {
        // The `UnixTime` sentinel (entries from pre-v4 archives).
        let out = render(&mk("foo"), &Extras::default());
        assert_not_contains!(out, "Last Updated");
    }

    #[test]
    fn split_members_render_one_per_line_with_their_desc() {
        let mut e = mk("bisq");
        e.pkgnames = vec![
            member("bisq-desktop", Some("Desktop client")),
            member("bisq-cli", None),
        ];
        let out = render(&e, &Extras::default());
        assert_contains!(out, "Split pkgs      : bisq-desktop: Desktop client\n");
        assert_contains!(out, "                  bisq-cli\n");
    }

    #[test]
    fn split_installed_sizes_are_labelled_per_member() {
        let mut e = mk("bisq");
        e.pkgnames = vec![member("bisq-desktop", None), member("bisq-cli", None)];
        let x = Extras {
            installed: vec![
                InstalledMember {
                    name: PkgName::new("bisq-desktop"),
                    size: ByteSize::new(210 * 1024 * 1024),
                },
                InstalledMember {
                    name: PkgName::new("bisq-cli"),
                    size: ByteSize::new(1024 * 1024),
                },
            ],
            ..Default::default()
        };
        let out = render(&e, &x);
        assert_contains!(out, "Installed Size  : bisq-desktop: 210.00 MiB\n");
        assert_contains!(out, "                  bisq-cli: 1.00 MiB\n");
    }

    #[test]
    fn maintainer_comments_parse_case_insensitively_in_file_order() {
        let pkgbuild = "\
# maintainer: First <first@example.org>
#Maintainer : Second <second@example.org>
# Contributor: Old Hand <old@example.org>
# maintainership notes: not a person
pkgname=foo
# Maintainer: buried mid-file counts too
";
        assert_eq!(
            maintainers(pkgbuild),
            vec![
                Maintainer::new("First <first@example.org>"),
                Maintainer::new("Second <second@example.org>"),
                Maintainer::new("buried mid-file counts too"),
            ]
        );
    }

    #[test]
    fn maintainer_without_colon_or_value_is_skipped() {
        assert!(maintainers("# Maintainer\n# Maintainer:\n# Maintainer:   \n").is_empty());
    }

    #[test]
    fn every_label_fits_the_16_column_gutter() {
        for l in Label::ALL {
            assert!(
                l.text().len() <= 16,
                "label {l:?} ({:?}) overflows the value column",
                l.text()
            );
        }
    }
}
