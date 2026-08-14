//! Composer-compatible version and constraint parsing.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// Normalized Composer package version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComposerVersion {
    /// Original string as it appears in lock/metadata (e.g. `v1.2.3`, `dev-main`).
    pub raw: String,
    /// Normalized comparable form without leading `v`.
    normalized: String,
    stability: Stability,
    /// Numeric components for comparison (major.minor.patch).
    parts: (u64, u64, u64),
    /// Pre-release label (alpha/beta/RC/dev number).
    pre: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Stability {
    Dev = 0,
    Alpha = 1,
    Beta = 2,
    Rc = 3,
    Stable = 4,
}

impl Stability {
    pub fn parse(s: &str) -> Self {
        let lower = s.to_ascii_lowercase();
        if lower.starts_with("dev") {
            Self::Dev
        } else if lower.starts_with("alpha") || lower.starts_with('a') && lower.len() > 1 {
            Self::Alpha
        } else if lower.starts_with("beta") || lower.starts_with('b') && lower.len() > 1 {
            Self::Beta
        } else if lower.starts_with("rc") {
            Self::Rc
        } else {
            Self::Stable
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Alpha => "alpha",
            Self::Beta => "beta",
            Self::Rc => "RC",
            Self::Stable => "stable",
        }
    }
}

impl ComposerVersion {
    pub fn parse(s: &str) -> Result<Self> {
        let raw = s.trim().to_string();
        if raw.is_empty() {
            return Err(Error::InvalidVersion(raw));
        }

        // Branch-like versions: dev-main, dev-master, 1.0.x-dev
        if raw.starts_with("dev-") || raw.ends_with("-dev") {
            return Ok(Self {
                raw: raw.clone(),
                normalized: raw.clone(),
                stability: Stability::Dev,
                parts: (0, 0, 0),
                pre: Some(raw.clone()),
            });
        }

        let without_v = raw.strip_prefix('v').unwrap_or(raw.as_str()).to_string();
        let (num_part, pre) = split_prerelease(&without_v);
        let parts = parse_numeric_parts(num_part)?;
        let stability = pre
            .as_ref()
            .map(|p| Stability::parse(p))
            .unwrap_or(Stability::Stable);

        Ok(Self {
            raw,
            normalized: without_v,
            stability,
            parts,
            pre,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    pub fn stability(&self) -> Stability {
        self.stability
    }

    pub fn is_stable(&self) -> bool {
        self.stability == Stability::Stable
    }

    /// Numeric components (major, minor, patch) used for range bounds.
    pub fn numeric_parts(&self) -> (u64, u64, u64) {
        self.parts
    }

    /// Composer-style 4-part `version_normalized` (e.g. `1.2.3` → `1.2.3.0`).
    pub fn version_normalized_composer(&self) -> String {
        if self.stability == Stability::Dev {
            return self.normalized.clone();
        }
        let (maj, min, pat) = self.parts;
        let mut s = format!("{maj}.{min}.{pat}.0");
        if let Some(pre) = &self.pre {
            // e.g. 1.0.0-beta1 → 1.0.0.0-beta1 (approximation)
            s.push('-');
            s.push_str(pre);
        }
        s
    }
}

/// Normalize a raw version string the way Composer stores `version_normalized`.
pub fn version_normalized(raw: &str) -> String {
    ComposerVersion::parse(raw)
        .map(|v| v.version_normalized_composer())
        .unwrap_or_else(|_| raw.trim().trim_start_matches('v').to_string())
}

impl PartialOrd for ComposerVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ComposerVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Dev branches sort below everything numeric when raw differs.
        match (self.stability, other.stability) {
            (Stability::Dev, Stability::Dev) => self.raw.cmp(&other.raw),
            (Stability::Dev, _) => Ordering::Less,
            (_, Stability::Dev) => Ordering::Greater,
            _ => self
                .parts
                .cmp(&other.parts)
                .then_with(|| self.stability.cmp(&other.stability))
                .then_with(|| self.pre.cmp(&other.pre)),
        }
    }
}

impl fmt::Display for ComposerVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for ComposerVersion {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

fn split_prerelease(s: &str) -> (&str, Option<String>) {
    // 1.2.3-beta1, 1.2.3RC1, 1.2.3-alpha
    if let Some(idx) = s.find('-') {
        return (&s[..idx], Some(s[idx + 1..].to_string()));
    }
    // 1.2.3RC1 / 1.2.3alpha1
    for marker in ["RC", "rc", "alpha", "ALPHA", "beta", "BETA", "a", "b"] {
        if let Some(idx) = s.find(marker) {
            // Only treat as pre if after a digit
            if idx > 0 && s.as_bytes()[idx - 1].is_ascii_digit() {
                return (&s[..idx], Some(s[idx..].to_string()));
            }
        }
    }
    (s, None)
}

fn parse_numeric_parts(s: &str) -> Result<(u64, u64, u64)> {
    let mut parts = s.split('.');
    let major = parts
        .next()
        .unwrap_or("0")
        .parse::<u64>()
        .map_err(|_| Error::InvalidVersion(s.to_string()))?;
    let minor = parts
        .next()
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);
    let patch = parts
        .next()
        .map(|p| {
            // strip non-digit suffix
            let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u64>().unwrap_or(0)
        })
        .unwrap_or(0);
    Ok((major, minor, patch))
}

/// Composer version constraint (e.g. `^2.0`, `~1.2`, `>=1.0 <2.0`, `*`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionConstraint {
    raw: String,
}

impl VersionConstraint {
    pub fn new(constraint: impl Into<String>) -> Self {
        Self {
            raw: constraint.into(),
        }
    }

    pub fn any() -> Self {
        Self::new("*")
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether `version` satisfies this constraint.
    pub fn matches(&self, version: &ComposerVersion) -> bool {
        let s = self.raw.trim();
        if s.is_empty() || s == "*" {
            return true;
        }

        // OR groups
        if s.contains("||") {
            return s
                .split("||")
                .any(|part| VersionConstraint::new(part.trim()).matches(version));
        }
        // Single | is also OR in Composer
        if s.contains('|') && !s.contains("||") {
            return s
                .split('|')
                .any(|part| VersionConstraint::new(part.trim()).matches(version));
        }

        // AND: comma and/or whitespace. Composer allows a space after operators
        // (`>= 7.1`); those must stay one clause, not `>=` AND `7.1`.
        split_constraint_and_clauses(s)
            .iter()
            .all(|c| match_single(c, version))
    }
}

impl Default for VersionConstraint {
    fn default() -> Self {
        Self::any()
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for VersionConstraint {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(Self::new(s))
    }
}

fn is_constraint_operator(tok: &str) -> bool {
    matches!(
        tok,
        ">=" | "<=" | "!=" | "<>" | ">" | "<" | "=" | "==" | "^" | "~"
    )
}

/// Split a Composer AND-constraint into clauses.
///
/// Commas always separate clauses. Whitespace also separates **complete**
/// clauses (`>=1.0 <2.0`), but a lone operator is glued to the next token
/// so `>= 7.1` and `>= 1.0 < 2.0` stay valid.
pub(crate) fn split_constraint_and_clauses(s: &str) -> Vec<String> {
    let mut raw: Vec<&str> = Vec::new();
    if s.contains(',') {
        for part in s.split(',') {
            raw.extend(part.split_whitespace());
        }
    } else {
        raw.extend(s.split_whitespace());
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if is_constraint_operator(raw[i]) && i + 1 < raw.len() {
            out.push(format!("{}{}", raw[i], raw[i + 1]));
            i += 2;
        } else if i + 2 < raw.len()
            && raw[i + 1] == "-"
            && !is_constraint_operator(raw[i])
            && !is_constraint_operator(raw[i + 2])
        {
            // Composer hyphen range: `8.1 - 8.5` (spaces around `-` required).
            out.push(format!("{} - {}", raw[i], raw[i + 2]));
            i += 3;
        } else if !raw[i].is_empty() {
            out.push(raw[i].to_string());
            i += 1;
        } else {
            i += 1;
        }
    }
    if out.is_empty() && !s.trim().is_empty() {
        out.push(s.trim().to_string());
    }
    out
}

fn match_single(clause: &str, version: &ComposerVersion) -> bool {
    let c = clause.trim();
    if c.is_empty() || c == "*" {
        return true;
    }

    // Stability flag suffix: package@dev — handled at dependency level; ignore here.
    let c = c.split('@').next().unwrap_or(c);

    if let Some((from, to)) = split_hyphen_range(c) {
        return match_hyphen_range(from, to, version);
    }
    if let Some(rest) = c.strip_prefix("^") {
        return match_caret(rest, version);
    }
    if let Some(rest) = c.strip_prefix('~') {
        return match_tilde(rest, version);
    }
    if c.ends_with(".*") || c.ends_with(".x") {
        return match_wildcard(c, version);
    }
    if let Some(rest) = c.strip_prefix(">=") {
        return cmp_ge(version, rest);
    }
    if let Some(rest) = c.strip_prefix("<=") {
        return cmp_le(version, rest);
    }
    if let Some(rest) = c.strip_prefix("!=") {
        return !versions_equal(version, rest);
    }
    if let Some(rest) = c.strip_prefix("<>") {
        return !versions_equal(version, rest);
    }
    if let Some(rest) = c.strip_prefix("==") {
        return versions_equal(version, rest);
    }
    if let Some(rest) = c.strip_prefix('>') {
        return cmp_gt(version, rest);
    }
    if let Some(rest) = c.strip_prefix('<') {
        return cmp_lt(version, rest);
    }
    if let Some(rest) = c.strip_prefix('=') {
        return versions_equal(version, rest);
    }

    // Bare version = exact (or prefix for incomplete versions)
    versions_equal(version, c)
}

fn parse_ref(s: &str) -> Option<ComposerVersion> {
    ComposerVersion::parse(s.trim().trim_start_matches('v')).ok()
}

/// Composer hyphen range `from - to` (spaces around `-` required).
fn split_hyphen_range(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let mid = s.find(" - ")?;
    let from = s[..mid].trim();
    let to = s[mid + 3..].trim();
    if from.is_empty() || to.is_empty() {
        return None;
    }
    if from.starts_with(['>', '<', '=', '^', '~', '!']) {
        return None;
    }
    Some((from, to))
}

/// How many numeric dotted components the spec wrote (`8.5` → 2, `8.5.0` → 3).
fn numeric_component_count(spec: &str) -> usize {
    spec.trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .count()
}

fn version_from_parts(maj: u64, min: u64, pat: u64) -> ComposerVersion {
    ComposerVersion {
        raw: format!("{maj}.{min}.{pat}"),
        normalized: format!("{maj}.{min}.{pat}"),
        stability: Stability::Stable,
        parts: (maj, min, pat),
        pre: None,
    }
}

/// Inclusive lower + upper bound. `exclusive` means `< upper`, else `<= upper`.
///
/// Composer: partial `to` (`8` / `8.5`) includes that whole branch (`<9` / `<8.6`);
/// a full `8.5.0` is inclusive (`<=8.5.0`).
pub(crate) fn hyphen_range_bounds(
    from: &str,
    to: &str,
) -> Option<(ComposerVersion, ComposerVersion, bool)> {
    let lower = parse_ref(from)?;
    let to_base = parse_ref(to)?;
    let (maj, min, _) = to_base.parts;
    let (upper, exclusive) = match numeric_component_count(to) {
        1 => (version_from_parts(maj.checked_add(1)?, 0, 0), true),
        2 => (version_from_parts(maj, min.checked_add(1)?, 0), true),
        0 => return None,
        _ => (to_base, false),
    };
    Some((lower, upper, exclusive))
}

fn match_hyphen_range(from: &str, to: &str, version: &ComposerVersion) -> bool {
    let Some((lower, upper, exclusive)) = hyphen_range_bounds(from, to) else {
        return false;
    };
    if version < &lower {
        return false;
    }
    if exclusive {
        version < &upper
    } else {
        version <= &upper
    }
}

fn versions_equal(version: &ComposerVersion, spec: &str) -> bool {
    let Some(other) = parse_ref(spec) else {
        return version.normalized == spec.trim_start_matches('v');
    };
    version.parts == other.parts && version.stability == other.stability
}

fn cmp_ge(version: &ComposerVersion, spec: &str) -> bool {
    let Some(other) = parse_ref(spec) else {
        return false;
    };
    version >= &other
}

fn cmp_le(version: &ComposerVersion, spec: &str) -> bool {
    let Some(other) = parse_ref(spec) else {
        return false;
    };
    version <= &other
}

fn cmp_gt(version: &ComposerVersion, spec: &str) -> bool {
    let Some(other) = parse_ref(spec) else {
        return false;
    };
    version > &other
}

fn cmp_lt(version: &ComposerVersion, spec: &str) -> bool {
    let Some(other) = parse_ref(spec) else {
        return false;
    };
    version < &other
}

/// Caret: `^1.2.3` => `>=1.2.3 <2.0.0`; `^0.2.3` => `>=0.2.3 <0.3.0`
fn match_caret(spec: &str, version: &ComposerVersion) -> bool {
    let Some(base) = parse_ref(spec) else {
        return false;
    };
    if version < &base {
        return false;
    }
    let (maj, min, _) = base.parts;
    let upper = if maj > 0 {
        ComposerVersion {
            raw: format!("{}.0.0", maj + 1),
            normalized: format!("{}.0.0", maj + 1),
            stability: Stability::Stable,
            parts: (maj + 1, 0, 0),
            pre: None,
        }
    } else if min > 0 {
        ComposerVersion {
            raw: format!("0.{}.0", min + 1),
            normalized: format!("0.{}.0", min + 1),
            stability: Stability::Stable,
            parts: (0, min + 1, 0),
            pre: None,
        }
    } else {
        ComposerVersion {
            raw: "0.0.1".into(),
            normalized: "0.0.1".into(),
            stability: Stability::Stable,
            parts: (0, 0, base.parts.2 + 1),
            pre: None,
        }
    };
    version < &upper
}

/// Tilde: `~1.2.3` => `>=1.2.3 <1.3.0`; `~1.2` => `>=1.2.0 <2.0.0`
fn match_tilde(spec: &str, version: &ComposerVersion) -> bool {
    let Some(base) = parse_ref(spec) else {
        return false;
    };
    if version < &base {
        return false;
    }
    let dots = spec.trim_start_matches('v').matches('.').count();
    let (maj, min, _) = base.parts;
    let upper = if dots >= 2 {
        // ~1.2.3 => <1.3.0
        ComposerVersion {
            raw: format!("{}.{}.0", maj, min + 1),
            normalized: format!("{}.{}.0", maj, min + 1),
            stability: Stability::Stable,
            parts: (maj, min + 1, 0),
            pre: None,
        }
    } else {
        // ~1.2 => <2.0.0
        ComposerVersion {
            raw: format!("{}.0.0", maj + 1),
            normalized: format!("{}.0.0", maj + 1),
            stability: Stability::Stable,
            parts: (maj + 1, 0, 0),
            pre: None,
        }
    };
    version < &upper
}

/// Wildcard: `1.2.*` / `1.*`
fn match_wildcard(spec: &str, version: &ComposerVersion) -> bool {
    let prefix = spec
        .trim_end_matches(".*")
        .trim_end_matches(".x")
        .trim_start_matches('v');
    let parts: Vec<&str> = prefix.split('.').collect();
    match parts.len() {
        1 => {
            let maj: u64 = parts[0].parse().unwrap_or(0);
            version.parts.0 == maj
        }
        2 => {
            let maj: u64 = parts[0].parse().unwrap_or(0);
            let min: u64 = parts[1].parse().unwrap_or(0);
            version.parts.0 == maj && version.parts.1 == min
        }
        _ => version.normalized.starts_with(prefix),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> ComposerVersion {
        ComposerVersion::parse(s).unwrap()
    }

    #[test]
    fn composer_version_normalized_four_parts() {
        assert_eq!(version_normalized("1.2.3"), "1.2.3.0");
        assert_eq!(version_normalized("v2.0.0"), "2.0.0.0");
        assert_eq!(version_normalized("1.0"), "1.0.0.0");
        assert!(version_normalized("dev-main").contains("dev-main"));
    }

    #[test]
    fn caret() {
        let c = VersionConstraint::new("^1.2.3");
        assert!(c.matches(&v("1.2.3")));
        assert!(c.matches(&v("1.9.0")));
        assert!(!c.matches(&v("2.0.0")));
        assert!(!c.matches(&v("1.2.2")));
    }

    #[test]
    fn tilde() {
        let c = VersionConstraint::new("~1.2.3");
        assert!(c.matches(&v("1.2.3")));
        assert!(c.matches(&v("1.2.9")));
        assert!(!c.matches(&v("1.3.0")));
    }

    #[test]
    fn range() {
        let c = VersionConstraint::new(">=1.0 <2.0");
        assert!(c.matches(&v("1.5.0")));
        assert!(!c.matches(&v("2.0.0")));
    }

    #[test]
    fn hyphen_range_partial_to_includes_branch() {
        let c = VersionConstraint::new("8.1 - 8.5");
        assert!(c.matches(&v("8.1.0")));
        assert!(c.matches(&v("8.5.0")));
        assert!(c.matches(&v("8.5.9")));
        assert!(!c.matches(&v("8.0.99")));
        assert!(!c.matches(&v("8.6.0")));

        let c = VersionConstraint::new("8.1.0 - 8.5.0");
        assert!(c.matches(&v("8.5.0")));
        assert!(!c.matches(&v("8.5.1")));

        let c = VersionConstraint::new("1 - 2");
        assert!(c.matches(&v("2.9.0")));
        assert!(!c.matches(&v("3.0.0")));

        assert_eq!(
            split_constraint_and_clauses("8.1 - 8.5"),
            vec!["8.1 - 8.5".to_string()]
        );
    }

    #[test]
    fn spaced_comparison_operators() {
        let c = VersionConstraint::new(">= 7.1");
        assert!(c.matches(&v("7.1.0")));
        assert!(c.matches(&v("8.5.9")));
        assert!(!c.matches(&v("7.0.33")));

        let c = VersionConstraint::new(">= 1.0 < 2.0");
        assert!(c.matches(&v("1.5.0")));
        assert!(!c.matches(&v("2.0.0")));

        let c = VersionConstraint::new(">= 7.1, < 8.0");
        assert!(c.matches(&v("7.4.0")));
        assert!(!c.matches(&v("8.0.0")));

        assert_eq!(
            split_constraint_and_clauses(">= 7.1"),
            vec![">=7.1".to_string()]
        );
        assert_eq!(
            split_constraint_and_clauses(">= 1.0 < 2.0"),
            vec![">=1.0".to_string(), "<2.0".to_string()]
        );
    }

    #[test]
    fn or_constraint() {
        let c = VersionConstraint::new("^1.0 || ^2.0");
        assert!(c.matches(&v("1.5.0")));
        assert!(c.matches(&v("2.1.0")));
        assert!(!c.matches(&v("3.0.0")));
    }

    #[test]
    fn ordering() {
        assert!(v("1.0.0") < v("2.0.0"));
        assert!(v("1.0.0-beta") < v("1.0.0"));
    }
}
