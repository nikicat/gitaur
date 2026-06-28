//! Selector parsing + resolution.
//!
//! Turns the arguments of `add` / `info` / `drop` / `review` into concrete
//! package targets. Each argument is one of:
//!
//! - a **number** (`3`) — the 1-based row of the most recent result list;
//! - a **range** (`5-8`) — inclusive, over the current list;
//! - a **name** (`glibc`) — a literal package name, passed through verbatim;
//! - a **glob** (`python-*`, `firefox?`) — matched against the name universe.
//!
//! Numbers/ranges index the current list; names/globs resolve against the
//! universe (AUR + sync-repo names). The result is order-preserving and
//! de-duplicated. This is the reusable core every cart-staging verb shares; it
//! is pure (no I/O), so it's exhaustively unit-tested here.

use super::ListItem;
use crate::names::PkgTarget;
use regex::Regex;
use std::collections::HashSet;

/// One parsed selector argument.
#[derive(Debug)]
enum Selector {
    /// 1-based row in the current list.
    Index(usize),
    /// Inclusive 1-based range over the current list.
    Range(usize, usize),
    /// Literal package name, passed through unresolved.
    Name(String),
    /// Wildcard, compiled to an anchored regex over the name universe.
    Glob(Regex),
}

/// Resolve `args` against the current `list` and the `universe` of names.
///
/// `Err` is reserved for hard errors (a malformed token, or a number/range
/// that falls outside the current list). A glob that matches nothing is *not*
/// an error — it simply contributes no targets — so the caller distinguishes
/// "bad input" from "valid input, nothing matched" by checking the returned
/// vector for emptiness.
pub fn resolve(
    args: &[String],
    list: &[ListItem],
    universe: &[PkgTarget],
) -> Result<Vec<PkgTarget>, String> {
    let mut raw: Vec<PkgTarget> = Vec::new();
    for tok in args {
        match parse_one(tok)? {
            Selector::Index(n) => raw.push(row(list, n)?),
            Selector::Range(a, b) => raw.extend(rows(list, a, b)?),
            Selector::Name(s) => raw.push(PkgTarget::new(s)),
            Selector::Glob(re) => {
                raw.extend(universe.iter().filter(|t| re.is_match(t.as_str())).cloned());
            }
        }
    }
    // Order-preserving de-dup: the first mention of a target wins its position.
    let mut seen = HashSet::new();
    Ok(raw.into_iter().filter(|t| seen.insert(t.clone())).collect())
}

/// Classify one token. Order matters: globs first (a `*`/`?` token is never a
/// name or number), then ranges (`N-M`, both sides all-digits), then a bare
/// number, then a literal name. Package names containing `-` (e.g. `yay-bin`)
/// fall through to [`Selector::Name`] because they aren't `digits-digits`.
fn parse_one(tok: &str) -> Result<Selector, String> {
    if tok.is_empty() {
        return Err("empty selector".into());
    }
    if is_glob(tok) {
        return Ok(Selector::Glob(glob_to_regex(tok)?));
    }
    if let Some(sel) = parse_range(tok)? {
        return Ok(sel);
    }
    if tok.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(Selector::Index(parse_index(tok)?));
    }
    Ok(Selector::Name(tok.to_owned()))
}

/// `digits-digits` → a range; anything else → `None` (fall through to name).
fn parse_range(tok: &str) -> Result<Option<Selector>, String> {
    let Some((lhs, rhs)) = tok.split_once('-') else {
        return Ok(None);
    };
    if lhs.is_empty()
        || rhs.is_empty()
        || !lhs.bytes().all(|b| b.is_ascii_digit())
        || !rhs.bytes().all(|b| b.is_ascii_digit())
    {
        return Ok(None);
    }
    let a = parse_index(lhs)?;
    let b = parse_index(rhs)?;
    if a > b {
        return Err(format!("range {tok}: start is past end"));
    }
    Ok(Some(Selector::Range(a, b)))
}

/// Parse a 1-based index, rejecting `0`.
fn parse_index(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|e| format!("not a number `{s}`: {e}"))?;
    if n == 0 {
        return Err("indices are 1-based; 0 is out of range".into());
    }
    Ok(n)
}

fn is_glob(tok: &str) -> bool {
    tok.contains(['*', '?'])
}

/// Translate a `*`/`?` glob into an anchored, case-sensitive regex. Everything
/// except the two wildcards is matched literally (regex metacharacters are
/// escaped first, then the escaped wildcards are rewritten).
fn glob_to_regex(glob: &str) -> Result<Regex, String> {
    let mut pat = String::with_capacity(glob.len() + 2);
    pat.push('^');
    for ch in glob.chars() {
        match ch {
            '*' => pat.push_str(".*"),
            '?' => pat.push('.'),
            other => pat.push_str(&regex::escape(&other.to_string())),
        }
    }
    pat.push('$');
    Regex::new(&pat).map_err(|e| format!("bad pattern `{glob}`: {e}"))
}

/// One row by 1-based index, or a descriptive error.
fn row(list: &[ListItem], n: usize) -> Result<PkgTarget, String> {
    if list.is_empty() {
        return Err("no current list — run `search` first".into());
    }
    list.get(n - 1)
        .map(|it| it.target.clone())
        .ok_or_else(|| format!("no row {n} (list has {})", list.len()))
}

/// A 1-based inclusive range of rows.
fn rows(list: &[ListItem], a: usize, b: usize) -> Result<Vec<PkgTarget>, String> {
    (a..=b).map(|n| row(list, n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(names: &[&str]) -> Vec<ListItem> {
        names
            .iter()
            .map(|n| ListItem {
                target: PkgTarget::new(*n),
                label: format!("aur/{n} 1-1"),
                repo: None,
            })
            .collect()
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn universe(parts: &[&str]) -> Vec<PkgTarget> {
        parts.iter().map(|s| PkgTarget::new(*s)).collect()
    }

    fn targets(v: &[PkgTarget]) -> Vec<String> {
        v.iter().map(|t| t.clone().into_inner()).collect()
    }

    #[test]
    fn number_indexes_the_current_list() {
        let l = list(&["foo", "bar", "baz"]);
        let got = resolve(&args(&["2"]), &l, &[]).unwrap();
        assert_eq!(targets(&got), vec!["bar"]);
    }

    #[test]
    fn range_is_inclusive() {
        let l = list(&["a", "b", "c", "d"]);
        let got = resolve(&args(&["2-4"]), &l, &[]).unwrap();
        assert_eq!(targets(&got), vec!["b", "c", "d"]);
    }

    #[test]
    fn literal_name_passes_through_even_if_not_in_universe() {
        let got = resolve(&args(&["some-aur-pkg"]), &[], &[]).unwrap();
        assert_eq!(targets(&got), vec!["some-aur-pkg"]);
    }

    #[test]
    fn hyphenated_name_is_not_a_range() {
        // `yay-bin` must resolve as a name, not be misread as a `N-M` range.
        let got = resolve(&args(&["yay-bin"]), &[], &[]).unwrap();
        assert_eq!(targets(&got), vec!["yay-bin"]);
    }

    #[test]
    fn glob_matches_the_universe_in_universe_order() {
        let universe = universe(&["python-bar", "python-foo", "ruby"]);
        let got = resolve(&args(&["python-*"]), &[], &universe).unwrap();
        assert_eq!(targets(&got), vec!["python-bar", "python-foo"]);
    }

    #[test]
    fn question_mark_glob_matches_single_char() {
        let universe = universe(&["gtk", "gtk2", "gtk3", "gtk-extra"]);
        let got = resolve(&args(&["gtk?"]), &[], &universe).unwrap();
        assert_eq!(targets(&got), vec!["gtk2", "gtk3"]);
    }

    #[test]
    fn glob_with_no_match_is_empty_not_error() {
        let universe = universe(&["foo", "bar"]);
        let got = resolve(&args(&["zzz-*"]), &[], &universe).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn mixed_selectors_dedup_preserving_first_position() {
        let l = list(&["foo", "bar"]);
        let universe = universe(&["foo", "bar", "baz"]);
        // `1` → foo, `bar` literal (dup of nothing yet), `*` → foo,bar,baz;
        // foo+bar already seen, so only baz is new.
        let got = resolve(&args(&["1", "bar", "*"]), &l, &universe).unwrap();
        assert_eq!(targets(&got), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn index_out_of_range_errors() {
        let l = list(&["only"]);
        assert!(resolve(&args(&["5"]), &l, &[]).is_err());
    }

    #[test]
    fn index_zero_errors() {
        let l = list(&["a"]);
        assert!(resolve(&args(&["0"]), &l, &[]).is_err());
    }

    #[test]
    fn number_with_empty_list_errors_helpfully() {
        let err = resolve(&args(&["1"]), &[], &[]).unwrap_err();
        assert!(err.contains("search"), "should hint to run search: {err}");
    }

    #[test]
    fn reversed_range_errors() {
        let l = list(&["a", "b", "c"]);
        assert!(resolve(&args(&["3-1"]), &l, &[]).is_err());
    }
}
