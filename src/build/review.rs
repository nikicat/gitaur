//! PKGBUILD review UX: full file on first install, diff against last build on update.
//!
//! Diff uses the bare mirror repo's object DB (not a `.git` inside the
//! worktree) — the build directory is just materialized files.

use crate::build::state_db::{BuildRecord, StateDb};
use crate::error::{Error, Result};
use crate::mirror::worktree::Worktree;
use crate::mirror::MirrorRepo;
use crate::ui;
use console::style;
use dialoguer::Select;
use gix::ObjectId;
use std::process::Command;
use tracing::{debug, info, instrument};

/// Drive the review prompt loop for one pkgbase.
#[instrument(skip(db, mirror, wt))]
pub fn review(
    db: &StateDb,
    mirror: &MirrorRepo,
    pkgbase: &str,
    wt: &Worktree,
    noconfirm: bool,
) -> Result<()> {
    let prior = db.get(pkgbase)?;
    if noconfirm {
        info!(
            pkgbase,
            prior = prior.is_some(),
            "auto-proceeding (noconfirm)"
        );
        return Ok(());
    }

    loop {
        show(mirror, pkgbase, wt, prior.as_ref())?;
        let choice = Select::new()
            .with_prompt(format!("[{pkgbase}] review"))
            .items(&["proceed", "view PKGBUILD", "edit", "skip", "abort"])
            .default(0)
            .interact()
            .map_err(|e| Error::other(format!("prompt: {e}")))?;
        match choice {
            0 => return Ok(()),
            1 => show_pkgbuild(wt)?,
            2 => edit_pkgbuild(wt)?,
            3 => return Err(Error::Build(format!("{pkgbase}: skipped"))),
            _ => return Err(Error::UserAbort),
        }
    }
}

fn show(
    mirror: &MirrorRepo,
    pkgbase: &str,
    wt: &Worktree,
    prior: Option<&BuildRecord>,
) -> Result<()> {
    match prior {
        None => {
            ui::step(&format!("first install: {pkgbase}"));
            show_pkgbuild(wt)?;
        }
        Some(prev) => {
            ui::step(&format!(
                "update: {pkgbase} (last built {})",
                prev.last_built_version
            ));
            show_diff(mirror, wt, &prev.last_built_commit_oid)?;
        }
    }
    Ok(())
}

fn show_pkgbuild(wt: &Worktree) -> Result<()> {
    let text = std::fs::read_to_string(wt.path.join("PKGBUILD"))?;
    print!("{}", highlight::pkgbuild(&text));
    Ok(())
}

fn edit_pkgbuild(wt: &Worktree) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let pkgbuild = wt.path.join("PKGBUILD");
    debug!(editor, file = %pkgbuild.display(), "launching editor");
    let status = Command::new(editor).arg(&pkgbuild).status()?;
    if !status.success() {
        return Err(Error::Build(format!("editor exited {:?}", status.code())));
    }
    Ok(())
}

/// Show a line-diff of `PKGBUILD` between the previously-built commit and
/// the freshly-materialized worktree's commit. Listing every other changed
/// path is left to the user — they have a real linked worktree, so plain
/// `git diff` works there.
fn show_diff(mirror: &MirrorRepo, wt: &Worktree, last_oid_hex: &str) -> Result<()> {
    let last_oid = ObjectId::from_hex(last_oid_hex.as_bytes())
        .map_err(|e| Error::Gix(format!("bad oid {last_oid_hex}: {e}")))?;
    let Ok(old_commit) = mirror.repo.find_commit(last_oid) else {
        ui::note("last-built commit not in mirror; showing full PKGBUILD");
        return show_pkgbuild(wt);
    };
    let new_commit = mirror
        .repo
        .find_commit(wt.head_oid)
        .map_err(|e| Error::Gix(format!("find_commit {}: {e}", wt.head_oid)))?;
    let old_tree = old_commit
        .tree()
        .map_err(|e| Error::Gix(format!("old tree: {e}")))?;
    let new_tree = new_commit
        .tree()
        .map_err(|e| Error::Gix(format!("new tree: {e}")))?;

    let old_text = read_pkgbuild(mirror, &old_tree)?;
    let new_text = read_pkgbuild(mirror, &new_tree)?;
    if old_text == new_text {
        ui::note("PKGBUILD unchanged since last build");
        return Ok(());
    }
    print_unified(&old_text, &new_text);
    Ok(())
}

fn read_pkgbuild(mirror: &MirrorRepo, tree: &gix::Tree<'_>) -> Result<String> {
    let entry = tree
        .find_entry("PKGBUILD")
        .ok_or_else(|| Error::Build("no PKGBUILD in tree".into()))?;
    let oid = entry.oid().to_owned();
    let blob = mirror
        .repo
        .find_object(oid)
        .map_err(|e| Error::Gix(format!("find PKGBUILD blob: {e}")))?;
    Ok(String::from_utf8_lossy(blob.data.as_slice()).into_owned())
}

fn print_unified(old: &str, new: &str) {
    use similar_minimal::diff;
    for op in diff(old, new) {
        match op {
            similar_minimal::Op::Keep(line) => println!(" {line}"),
            similar_minimal::Op::Add(line) => println!("{}", style(format!("+{line}")).green()),
            similar_minimal::Op::Remove(line) => println!("{}", style(format!("-{line}")).red()),
        }
    }
}

mod highlight {
    //! Bash syntax coloring for the PKGBUILD review screen, via `syntect`'s
    //! bundled Sublime grammar (same grammar `bat` uses for `.sh`/PKGBUILD).
    //!
    //! Loaded lazily — the bundled `SyntaxSet` costs ~100 ms to parse on first
    //! use, then is cached for the rest of the process. Any failure (theme
    //! missing, grammar unloadable, per-line highlight error) falls back to
    //! plain text rather than aborting review.
    use crate::ui;
    use std::sync::OnceLock;
    use syntect::easy::HighlightLines;
    use syntect::highlighting::{Theme, ThemeSet};
    use syntect::parsing::SyntaxSet;
    use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};

    struct Ctx {
        syntaxes: SyntaxSet,
        theme: Theme,
    }

    fn ctx() -> &'static Ctx {
        static CTX: OnceLock<Ctx> = OnceLock::new();
        CTX.get_or_init(|| Ctx {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: ThemeSet::load_defaults()
                .themes
                .remove("base16-ocean.dark")
                .expect("syntect ships base16-ocean.dark"),
        })
    }

    /// Render PKGBUILD source. Always ends with a single `\n` so the prompt
    /// that follows lands on a fresh line; passes `false` to the terminal
    /// escaper so the theme's background never paints over the user's bg.
    pub fn pkgbuild(text: &str) -> String {
        render(text, ui::color_on())
    }

    fn render(text: &str, colors: bool) -> String {
        if !colors {
            return plain(text);
        }
        try_color(text).unwrap_or_else(|| plain(text))
    }

    fn plain(text: &str) -> String {
        if text.is_empty() || text.ends_with('\n') {
            return text.to_owned();
        }
        let mut s = String::with_capacity(text.len() + 1);
        s.push_str(text);
        s.push('\n');
        s
    }

    fn try_color(text: &str) -> Option<String> {
        if text.is_empty() {
            return Some(String::new());
        }
        let Ctx { syntaxes, theme } = ctx();
        let syntax = syntaxes
            .find_syntax_by_name("Bourne Again Shell (bash)")
            .or_else(|| syntaxes.find_syntax_by_extension("sh"))?;
        let mut hl = HighlightLines::new(syntax, theme);
        let mut out = String::with_capacity(text.len() * 2);
        for line in LinesWithEndings::from(text) {
            let ranges = hl.highlight_line(line, syntaxes).ok()?;
            out.push_str(&as_24_bit_terminal_escaped(&ranges, false));
        }
        // Move any trailing newline past the reset so the styled block ends
        // with `\x1b[0m\n` regardless of whether the source had a final \n.
        if out.ends_with('\n') {
            out.pop();
        }
        out.push_str("\u{1b}[0m\n");
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::render;
        use console::strip_ansi_codes;

        #[test]
        fn colored_roundtrips_to_source() {
            let src = "pkgname=foo\npkgver=1.2.3\n\nbuild() {\n    cd \"$srcdir/$pkgname-$pkgver\"  # comment\n    make\n}\n";
            let out = render(src, true);
            assert!(out.contains("\u{1b}["), "expected ANSI escapes: {out:?}");
            assert!(out.ends_with("\u{1b}[0m\n"), "missing final reset+nl: {out:?}");
            // Strip the trailing reset before comparing, since strip_ansi_codes
            // leaves the surrounding text alone.
            assert_eq!(strip_ansi_codes(&out).trim_end_matches('\n'), src.trim_end_matches('\n'));
        }

        #[test]
        fn plain_when_colors_off() {
            let src = "pkgname=foo\n";
            assert_eq!(render(src, false), src);
        }

        #[test]
        fn adds_trailing_newline_when_source_lacks_one() {
            assert_eq!(render("pkgname=foo", false), "pkgname=foo\n");
            let out = render("pkgname=foo", true);
            assert!(out.ends_with("\u{1b}[0m\n"));
        }

        #[test]
        fn empty_input_stays_empty() {
            assert_eq!(render("", false), "");
            assert_eq!(render("", true), "");
        }

        #[test]
        fn utf8_in_pkgdesc_does_not_panic() {
            let src = "pkgdesc=\"héllo wörld — 漢字\"\n";
            let out = render(src, true);
            assert_eq!(strip_ansi_codes(&out).trim_end_matches('\n'), src.trim_end_matches('\n'));
        }
    }
}

mod similar_minimal {
    //! Tiny LCS-based unified-diff renderer (just enough for PKGBUILD review).
    //! We avoid pulling the full `similar` crate for this one use.

    pub enum Op {
        Keep(String),
        Add(String),
        Remove(String),
    }

    #[allow(clippy::many_single_char_names)] // standard LCS variable naming
    pub fn diff(a: &str, b: &str) -> Vec<Op> {
        let a: Vec<&str> = a.lines().collect();
        let b: Vec<&str> = b.lines().collect();
        let n = a.len();
        let m = b.len();
        // LCS table.
        let mut lcs = vec![vec![0u32; m + 1]; n + 1];
        for i in 0..n {
            for j in 0..m {
                lcs[i + 1][j + 1] = if a[i] == b[j] {
                    lcs[i][j] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        // Walk back to produce ops.
        let mut out = Vec::new();
        let (mut i, mut j) = (n, m);
        while i > 0 && j > 0 {
            if a[i - 1] == b[j - 1] {
                out.push(Op::Keep(a[i - 1].to_string()));
                i -= 1;
                j -= 1;
            } else if lcs[i][j - 1] >= lcs[i - 1][j] {
                out.push(Op::Add(b[j - 1].to_string()));
                j -= 1;
            } else {
                out.push(Op::Remove(a[i - 1].to_string()));
                i -= 1;
            }
        }
        while i > 0 {
            out.push(Op::Remove(a[i - 1].to_string()));
            i -= 1;
        }
        while j > 0 {
            out.push(Op::Add(b[j - 1].to_string()));
            j -= 1;
        }
        out.reverse();
        out
    }
}
