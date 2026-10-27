//! Versions and the ranges dependencies ask for, as semantic versioning has
//! them: `1.2.3`, with an optional pre-release (`1.2.3-beta.1`), and ranges
//! such as `^1.2`, `~1.2.3`, `=1.2.3`, `>=1.0, <2.0`, and `*`. A bare
//! version, `1.2.3`, means `^1.2.3`: that version or any later one that
//! does not change the first number that is not 0.

use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The pre-release, as `beta.1`; empty for a release.
    pub pre: String,
}

impl Version {
    /// Reads a whole version, all three numbers written.
    pub fn parse(text: &str) -> Option<Version> {
        let (numbers, pre) = match text.split_once('-') {
            Some((numbers, pre)) => (numbers, pre),
            None => (text, ""),
        };
        let parts: Vec<&str> = numbers.split('.').collect();
        let [major, minor, patch] = parts.as_slice() else {
            return None;
        };
        let number = |part: &str| -> Option<u64> {
            (!part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0')))
            .then(|| part.parse().ok())
            .flatten()
        };
        let pre_ok = pre.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        if text.contains('-') && (pre.is_empty() || !pre_ok) {
            return None;
        }
        Some(Version {
            major: number(major)?,
            minor: number(minor)?,
            patch: number(patch)?,
            pre: pre.to_string(),
        })
    }

    fn numbers(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre)?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    /// Semantic versioning's order: a pre-release comes before its release,
    /// and pre-releases compare identifier by identifier, numbers as numbers.
    fn cmp(&self, other: &Version) -> Ordering {
        self.numbers().cmp(&other.numbers()).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    let mut a = self.pre.split('.');
                    let mut b = other.pre.split('.');
                    loop {
                        match (a.next(), b.next()) {
                            (None, None) => return Ordering::Equal,
                            (None, Some(_)) => return Ordering::Less,
                            (Some(_), None) => return Ordering::Greater,
                            (Some(x), Some(y)) => {
                                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                                    (Ok(x), Ok(y)) => x.cmp(&y),
                                    (Ok(_), Err(_)) => Ordering::Less,
                                    (Err(_), Ok(_)) => Ordering::Greater,
                                    (Err(_), Err(_)) => x.cmp(y),
                                };
                                if order != Ordering::Equal {
                                    return order;
                                }
                            }
                        }
                    }
                }
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operator {
    Caret,
    Tilde,
    Exact,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
}

/// One part of a range: an operator and a version whose later numbers may
/// be left out, as `^1.2`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Comparator {
    operator: Operator,
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: String,
}

impl Comparator {
    fn lowest(&self) -> Version {
        Version {
            major: self.major,
            minor: self.minor.unwrap_or(0),
            patch: self.patch.unwrap_or(0),
            pre: self.pre.clone(),
        }
    }

    fn matches(&self, version: &Version) -> bool {
        let lowest = self.lowest();
        let numbers = version.numbers();
        match self.operator {
            Operator::Exact => match (self.minor, self.patch) {
                (Some(_), Some(_)) => *version == lowest,
                (Some(minor), None) => (version.major, version.minor) == (self.major, minor),
                _ => version.major == self.major,
            },
            Operator::Greater => match (self.minor, self.patch) {
                (Some(_), Some(_)) => *version > lowest,
                (Some(minor), None) => (version.major, version.minor) > (self.major, minor),
                _ => version.major > self.major,
            },
            Operator::GreaterEqual => *version >= lowest,
            Operator::Less => *version < lowest,
            Operator::LessEqual => match (self.minor, self.patch) {
                (Some(_), Some(_)) => *version <= lowest,
                (Some(minor), None) => (version.major, version.minor) <= (self.major, minor),
                _ => version.major <= self.major,
            },
            Operator::Tilde => {
                *version >= lowest
                    && match self.minor {
                        Some(minor) => (numbers.0, numbers.1) == (self.major, minor),
                        None => numbers.0 == self.major,
                    }
            }
            Operator::Caret => {
                if *version < lowest {
                    return false;
                }
                match (self.major, self.minor, self.patch) {
                    (0, Some(0), Some(patch)) => numbers == (0, 0, patch),
                    (0, Some(minor), _) => (numbers.0, numbers.1) == (0, minor),
                    (major, _, _) => numbers.0 == major,
                }
            }
        }
    }
}

/// A range of versions a dependency accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// All must match; none for `*`, which every release matches.
    comparators: Vec<Comparator>,
    text: String,
}

impl Requirement {
    pub fn parse(text: &str) -> Result<Requirement, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("a version range cannot be empty".to_string());
        }
        let mut comparators = Vec::new();
        if trimmed != "*" {
            for part in trimmed.split(',') {
                comparators.push(comparator(part.trim()).ok_or_else(|| {
                    format!(
                        "\"{}\" is not a version range, such as \"^1.2\" or \">=1.0, <2.0\"",
                        part.trim()
                    )
                })?);
            }
        }
        Ok(Requirement {
            comparators,
            text: trimmed.to_string(),
        })
    }

    /// Whether a version is in the range. A pre-release is only when the
    /// range names a pre-release of the same three numbers.
    pub fn matches(&self, version: &Version) -> bool {
        if !version.pre.is_empty()
            && !self.comparators.iter().any(|c| {
                !c.pre.is_empty()
                    && (c.major, c.minor, c.patch)
                        == (version.major, Some(version.minor), Some(version.patch))
            })
        {
            return false;
        }
        self.comparators.iter().all(|c| c.matches(version))
    }

    /// The latest of `versions` in the range.
    pub fn best<'v>(&self, versions: impl IntoIterator<Item = &'v Version>) -> Option<&'v Version> {
        versions.into_iter().filter(|v| self.matches(v)).max()
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn comparator(text: &str) -> Option<Comparator> {
    let (operator, rest) = [
        (">=", Operator::GreaterEqual),
        ("<=", Operator::LessEqual),
        (">", Operator::Greater),
        ("<", Operator::Less),
        ("=", Operator::Exact),
        ("^", Operator::Caret),
        ("~", Operator::Tilde),
    ]
    .iter()
    .find_map(|(prefix, operator)| {
        text.strip_prefix(prefix)
            .map(|rest| (*operator, rest.trim_start()))
    })
    .unwrap_or((Operator::Caret, text));
    let (numbers, pre) = match rest.split_once('-') {
        Some((numbers, pre)) => (numbers, pre.to_string()),
        None => (rest, String::new()),
    };
    let parts: Vec<&str> = numbers.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let number = |part: &str| -> Option<u64> {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    let major = number(parts[0])?;
    let minor = match parts.get(1) {
        Some(part) => Some(number(part)?),
        None => None,
    };
    let patch = match parts.get(2) {
        Some(part) => Some(number(part)?),
        None => None,
    };
    if !pre.is_empty() && patch.is_none() {
        return None;
    }
    Some(Comparator {
        operator,
        major,
        minor,
        patch,
        pre,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn accepts(range: &str, version: &str) -> bool {
        Requirement::parse(range).unwrap().matches(&v(version))
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(v("1.2.3").to_string(), "1.2.3");
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("01.2.3").is_none());
        assert!(Version::parse("1.2.3-").is_none());
        let mut versions = [
            v("1.0.0"),
            v("1.0.0-alpha"),
            v("1.0.0-alpha.2"),
            v("1.0.0-alpha.10"),
            v("0.9.9"),
        ];
        versions.sort();
        let shown: Vec<String> = versions.iter().map(Version::to_string).collect();
        assert_eq!(
            shown,
            [
                "0.9.9",
                "1.0.0-alpha",
                "1.0.0-alpha.2",
                "1.0.0-alpha.10",
                "1.0.0"
            ]
        );
    }

    #[test]
    fn ranges_accept_what_they_say() {
        assert!(
            accepts("1.2.3", "1.9.0") && !accepts("1.2.3", "2.0.0") && !accepts("1.2.3", "1.2.2")
        );
        assert!(accepts("^0.2", "0.2.9") && !accepts("^0.2", "0.3.0"));
        assert!(accepts("^0.0.3", "0.0.3") && !accepts("^0.0.3", "0.0.4"));
        assert!(accepts("~1.2.3", "1.2.9") && !accepts("~1.2.3", "1.3.0"));
        assert!(accepts("=1.2.3", "1.2.3") && !accepts("=1.2.3", "1.2.4"));
        assert!(accepts(">=1.0, <2.0", "1.5.0") && !accepts(">=1.0, <2.0", "2.0.0"));
        assert!(accepts("*", "7.1.0") && !accepts("*", "7.1.0-rc.1"));
        assert!(accepts("=2.0.0-rc.1", "2.0.0-rc.1") && !accepts("^1.0", "1.1.0-beta"));
        assert!(Requirement::parse("one point two").is_err());
        let versions = [v("1.0.0"), v("1.4.2"), v("2.0.0")];
        assert_eq!(
            Requirement::parse("^1").unwrap().best(&versions),
            Some(&versions[1])
        );
    }
}
