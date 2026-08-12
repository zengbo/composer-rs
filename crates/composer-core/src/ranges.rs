//! Convert Composer constraints into pubgrub `Ranges`.

use crate::version::{ComposerVersion, Stability, VersionConstraint};
use version_ranges::Ranges;

/// Convert a Composer version constraint into a `Ranges<ComposerVersion>`.
pub fn constraint_to_ranges(constraint: &VersionConstraint) -> Ranges<ComposerVersion> {
    let s = constraint.as_str().trim();
    if s.is_empty() || s == "*" {
        return Ranges::full();
    }

    if s.contains("||") {
        let mut acc = Ranges::empty();
        for part in s.split("||") {
            acc = acc.union(&constraint_to_ranges(&VersionConstraint::new(part.trim())));
        }
        return acc;
    }
    if s.contains('|') && !s.contains("||") {
        let mut acc = Ranges::empty();
        for part in s.split('|') {
            acc = acc.union(&constraint_to_ranges(&VersionConstraint::new(part.trim())));
        }
        return acc;
    }

    let clauses = split_and(s);
    let mut acc = Ranges::full();
    for c in clauses {
        acc = acc.intersection(&single_to_ranges(c));
    }
    acc
}

/// Allowed versions when a package `conflict`s with `constraint` (complement range).
pub fn conflict_to_ranges(constraint: &VersionConstraint) -> Ranges<ComposerVersion> {
    constraint_to_ranges(constraint).complement()
}

fn split_and(s: &str) -> Vec<&str> {
    if s.contains(',') {
        return s
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
    }
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            if start < i {
                out.push(s[start..i].trim());
            }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            start = i;
            continue;
        }
        i += 1;
    }
    if start < s.len() {
        let t = s[start..].trim();
        if !t.is_empty() {
            out.push(t);
        }
    }
    if out.is_empty() {
        out.push(s);
    }
    out
}

fn single_to_ranges(clause: &str) -> Ranges<ComposerVersion> {
    let c = clause.trim().split('@').next().unwrap_or(clause).trim();
    if c.is_empty() || c == "*" {
        return Ranges::full();
    }

    if let Some(rest) = c.strip_prefix("^") {
        return caret_range(rest);
    }
    if let Some(rest) = c.strip_prefix('~') {
        return tilde_range(rest);
    }
    if c.ends_with(".*") || c.ends_with(".x") {
        return wildcard_range(c);
    }
    if let Some(rest) = c.strip_prefix(">=") {
        return parse_ver(rest)
            .map(Ranges::higher_than)
            .unwrap_or_else(Ranges::full);
    }
    if let Some(rest) = c.strip_prefix("<=") {
        return parse_ver(rest)
            .map(Ranges::lower_than)
            .unwrap_or_else(Ranges::full);
    }
    if let Some(rest) = c.strip_prefix("!=") {
        return parse_ver(rest)
            .map(|v| Ranges::singleton(v).complement())
            .unwrap_or_else(Ranges::full);
    }
    if let Some(rest) = c.strip_prefix('>') {
        return parse_ver(rest)
            .map(Ranges::strictly_higher_than)
            .unwrap_or_else(Ranges::full);
    }
    if let Some(rest) = c.strip_prefix('<') {
        return parse_ver(rest)
            .map(Ranges::strictly_lower_than)
            .unwrap_or_else(Ranges::full);
    }
    if let Some(rest) = c.strip_prefix('=') {
        return parse_ver(rest)
            .map(Ranges::singleton)
            .unwrap_or_else(Ranges::full);
    }

    parse_ver(c)
        .map(Ranges::singleton)
        .unwrap_or_else(Ranges::full)
}

fn parse_ver(s: &str) -> Option<ComposerVersion> {
    ComposerVersion::parse(s.trim().trim_start_matches('v')).ok()
}

fn caret_range(spec: &str) -> Ranges<ComposerVersion> {
    let Some(base) = parse_ver(spec) else {
        return Ranges::full();
    };
    let (maj, min, pat) = parts(&base);
    let upper = if maj > 0 {
        ver(maj + 1, 0, 0)
    } else if min > 0 {
        ver(0, min + 1, 0)
    } else {
        ver(0, 0, pat + 1)
    };
    Ranges::between(base, upper)
}

fn tilde_range(spec: &str) -> Ranges<ComposerVersion> {
    let Some(base) = parse_ver(spec) else {
        return Ranges::full();
    };
    let dots = spec.trim_start_matches('v').matches('.').count();
    let (maj, min, _) = parts(&base);
    let upper = if dots >= 2 {
        ver(maj, min + 1, 0)
    } else {
        ver(maj + 1, 0, 0)
    };
    Ranges::between(base, upper)
}

fn wildcard_range(spec: &str) -> Ranges<ComposerVersion> {
    let prefix = spec
        .trim_end_matches(".*")
        .trim_end_matches(".x")
        .trim_start_matches('v');
    let segs: Vec<&str> = prefix.split('.').collect();
    match segs.len() {
        1 => {
            let maj: u64 = segs[0].parse().unwrap_or(0);
            Ranges::between(ver(maj, 0, 0), ver(maj + 1, 0, 0))
        }
        2 => {
            let maj: u64 = segs[0].parse().unwrap_or(0);
            let min: u64 = segs[1].parse().unwrap_or(0);
            Ranges::between(ver(maj, min, 0), ver(maj, min + 1, 0))
        }
        _ => Ranges::full(),
    }
}

fn parts(v: &ComposerVersion) -> (u64, u64, u64) {
    let n = v.normalized();
    let mut it = n.split(|c: char| !c.is_ascii_digit() && c != '.');
    let num = it.next().unwrap_or("0");
    let mut segs = num.split('.');
    let maj = segs.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let min = segs.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let pat = segs.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let _ = v.stability();
    (maj, min, pat)
}

fn ver(maj: u64, min: u64, pat: u64) -> ComposerVersion {
    ComposerVersion::parse(&format!("{maj}.{min}.{pat}"))
        .unwrap_or_else(|_| ComposerVersion::parse("0.0.0").unwrap())
}

/// Stability gate used when filtering candidate versions.
pub fn meets_min_stability(version: &ComposerVersion, min: Stability) -> bool {
    version.stability() >= min
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_contains() {
        let r = constraint_to_ranges(&VersionConstraint::new("^1.2.3"));
        assert!(r.contains(&ComposerVersion::parse("1.2.3").unwrap()));
        assert!(r.contains(&ComposerVersion::parse("1.9.0").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("2.0.0").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("1.2.2").unwrap()));
    }

    #[test]
    fn or_union() {
        let r = constraint_to_ranges(&VersionConstraint::new("^1.0 || ^2.0"));
        assert!(r.contains(&ComposerVersion::parse("1.5.0").unwrap()));
        assert!(r.contains(&ComposerVersion::parse("2.1.0").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("3.0.0").unwrap()));
    }

    #[test]
    fn conflict_complement_excludes_matching_versions() {
        let r = conflict_to_ranges(&VersionConstraint::new("^1.0"));
        assert!(!r.contains(&ComposerVersion::parse("1.2.0").unwrap()));
        assert!(r.contains(&ComposerVersion::parse("2.0.0").unwrap()));
    }
}
