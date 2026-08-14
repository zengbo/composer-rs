//! Convert Composer constraints into pubgrub `Ranges`.

use crate::version::{
    ComposerVersion, Stability, VersionConstraint, hyphen_range_bounds,
    split_constraint_and_clauses,
};
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

    let clauses = split_constraint_and_clauses(s);
    let mut acc = Ranges::full();
    for c in clauses {
        acc = acc.intersection(&single_to_ranges(&c));
    }
    acc
}

/// Allowed versions when a package `conflict`s with `constraint` (complement range).
pub fn conflict_to_ranges(constraint: &VersionConstraint) -> Ranges<ComposerVersion> {
    constraint_to_ranges(constraint).complement()
}

fn single_to_ranges(clause: &str) -> Ranges<ComposerVersion> {
    let c = clause.trim().split('@').next().unwrap_or(clause).trim();
    if c.is_empty() || c == "*" {
        return Ranges::full();
    }

    if let Some((from, to)) = c.split_once(" - ") {
        if let Some(r) = hyphen_to_ranges(from.trim(), to.trim()) {
            return r;
        }
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
    // Longer operators first: `==` / `<>` must not fall through to `=` / `<`.
    if let Some(rest) = c.strip_prefix("==") {
        return parse_ver(rest)
            .map(Ranges::singleton)
            .unwrap_or_else(Ranges::empty);
    }
    if let Some(rest) = c.strip_prefix("<>") {
        return parse_ver(rest)
            .map(|v| Ranges::singleton(v).complement())
            .unwrap_or_else(Ranges::empty);
    }
    if let Some(rest) = c.strip_prefix(">=") {
        return parse_ver(rest)
            .map(Ranges::higher_than)
            .unwrap_or_else(Ranges::empty);
    }
    if let Some(rest) = c.strip_prefix("<=") {
        return parse_ver(rest)
            .map(Ranges::lower_than)
            .unwrap_or_else(Ranges::empty);
    }
    if let Some(rest) = c.strip_prefix("!=") {
        return parse_ver(rest)
            .map(|v| Ranges::singleton(v).complement())
            .unwrap_or_else(Ranges::empty);
    }
    if let Some(rest) = c.strip_prefix('>') {
        return parse_ver(rest)
            .map(Ranges::strictly_higher_than)
            .unwrap_or_else(Ranges::empty);
    }
    if let Some(rest) = c.strip_prefix('<') {
        return parse_ver(rest)
            .map(Ranges::strictly_lower_than)
            .unwrap_or_else(Ranges::empty);
    }
    if let Some(rest) = c.strip_prefix('=') {
        return parse_ver(rest)
            .map(Ranges::singleton)
            .unwrap_or_else(Ranges::empty);
    }

    // Unparseable tokens must not become "any version".
    parse_ver(c)
        .map(Ranges::singleton)
        .unwrap_or_else(Ranges::empty)
}

fn parse_ver(s: &str) -> Option<ComposerVersion> {
    ComposerVersion::parse(s.trim().trim_start_matches('v')).ok()
}

fn hyphen_to_ranges(from: &str, to: &str) -> Option<Ranges<ComposerVersion>> {
    let (lower, upper, exclusive) = hyphen_range_bounds(from, to)?;
    let high = if exclusive {
        Ranges::strictly_lower_than(upper)
    } else {
        Ranges::lower_than(upper)
    };
    Some(Ranges::higher_than(lower).intersection(&high))
}

fn caret_range(spec: &str) -> Ranges<ComposerVersion> {
    let Some(base) = parse_ver(spec) else {
        return Ranges::empty();
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
        return Ranges::empty();
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
        _ => Ranges::empty(),
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

    #[test]
    fn spaced_ge_includes_newer_majors() {
        let r = constraint_to_ranges(&VersionConstraint::new(">= 7.1"));
        assert!(r.contains(&ComposerVersion::parse("7.1.0").unwrap()));
        assert!(r.contains(&ComposerVersion::parse("8.5.9").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("7.0.0").unwrap()));
    }

    #[test]
    fn hyphen_range_php_branch() {
        let r = constraint_to_ranges(&VersionConstraint::new("8.1 - 8.5"));
        assert!(r.contains(&ComposerVersion::parse("8.1.0").unwrap()));
        assert!(r.contains(&ComposerVersion::parse("8.5.9").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("8.0.0").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("8.6.0").unwrap()));
    }

    #[test]
    fn double_equals_is_exact() {
        let r = constraint_to_ranges(&VersionConstraint::new("== 1.0.0"));
        assert!(r.contains(&ComposerVersion::parse("1.0.0").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("2.0.0").unwrap()));
    }

    #[test]
    fn diamond_not_equal_excludes_version() {
        let r = constraint_to_ranges(&VersionConstraint::new("<> 2.0.0"));
        assert!(r.contains(&ComposerVersion::parse("1.0.0").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("2.0.0").unwrap()));
    }

    #[test]
    fn unparseable_operator_does_not_fail_open() {
        let r = constraint_to_ranges(&VersionConstraint::new("== not-a-version"));
        assert!(!r.contains(&ComposerVersion::parse("1.0.0").unwrap()));
        assert!(!r.contains(&ComposerVersion::parse("9.9.9").unwrap()));
    }
}
