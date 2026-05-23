//! Parser for pacman-style clustered flags (e.g. `-Syu` → `S`, `y`, `u`).

/// Parsed view of a pacman-style argument vector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PacFlags {
    /// Operation letter (first short flag): one of `S`, `Q`, `R`, ...
    pub op: Option<char>,
    /// Short flag modifiers (everything after the op letter, plus subsequent `-x` flags).
    pub op_letters: Vec<char>,
    /// Long-form flags without the leading `--`.
    pub long: Vec<String>,
    /// Non-flag positional arguments.
    pub positional: Vec<String>,
}

impl PacFlags {
    /// True if the short flag `c` appears as op or modifier.
    pub fn has(&self, c: char) -> bool {
        self.op == Some(c) || self.op_letters.contains(&c)
    }

    /// True if `--<s>` was passed.
    pub fn has_long(&self, s: &str) -> bool {
        self.long.iter().any(|l| l == s)
    }
}

/// Parse a flat argv into [`PacFlags`]. Does not validate flag legality.
pub fn parse(argv: &[String]) -> PacFlags {
    let mut f = PacFlags::default();
    for a in argv {
        if let Some(rest) = a.strip_prefix("--") {
            f.long.push(rest.to_owned());
        } else if let Some(rest) = a.strip_prefix('-') {
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                if f.op.is_none() {
                    f.op = Some(c);
                } else {
                    f.op_letters.push(c);
                }
                for c in chars {
                    f.op_letters.push(c);
                }
            }
        } else {
            f.positional.push(a.clone());
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cluster() {
        let f = parse(&["-Syu".into(), "--noconfirm".into(), "vim".into()]);
        assert_eq!(f.op, Some('S'));
        assert_eq!(f.op_letters, vec!['y', 'u']);
        assert!(f.has_long("noconfirm"));
        assert_eq!(f.positional, vec!["vim".to_owned()]);
    }

    #[test]
    fn parses_split() {
        let f = parse(&["-S".into(), "-y".into(), "foo".into()]);
        assert!(f.has('S'));
        assert!(f.has('y'));
        assert_eq!(f.positional, vec!["foo".to_owned()]);
    }
}
