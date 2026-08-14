//! `composer-rs outdated`

use super::{info, project_paths, warning};
use anyhow::{Result, bail};
use clap::Args;
use composer_core::{ComposerVersion, PackageId, VersionConstraint};
use composer_lock::{ComposerLock, LockedPackage};
use composer_manifest::ComposerJson;
use composer_repo::{RemotePackageVersion, RepositoryRegistry};

#[derive(Args, Debug, Clone)]
pub struct OutdatedArgs {
    /// Only root require / require-dev packages
    #[arg(long, short = 'D')]
    pub direct: bool,

    /// Show all packages including up-to-date (Composer `--all`)
    #[arg(long, short = 'a')]
    pub all: bool,

    /// Exit 1 if any package is outdated (Composer `--strict`)
    #[arg(long)]
    pub strict: bool,

    /// Only major (semver-breaking) updates
    #[arg(long, short = 'M')]
    pub major_only: bool,

    /// Only minor+patch (semver-compatible) updates
    #[arg(long, short = 'm')]
    pub minor_only: bool,

    /// Output format
    #[arg(long, default_value = "text")]
    pub format: String,

    #[arg(long)]
    pub no_dev: bool,
}

/// Composer-style outdated status for a package row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutdatedKind {
    /// Up to date (`=`)
    UpToDate,
    /// Semver-compatible update available (`!`) — current < wanted
    SemverSafe,
    /// Only a major update is available beyond constraints (`~`) — current == wanted < latest
    MajorOnly,
}

impl OutdatedKind {
    pub fn marker(self) -> char {
        match self {
            Self::UpToDate => '=',
            Self::SemverSafe => '!',
            Self::MajorOnly => '~',
        }
    }

    pub fn is_outdated(self) -> bool {
        !matches!(self, Self::UpToDate)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OutdatedRow {
    pub name: String,
    pub current: String,
    pub wanted: String,
    pub latest: String,
    pub status: OutdatedKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl OutdatedRow {
    pub fn is_outdated(&self) -> bool {
        self.status.is_outdated()
    }
}

fn parse_ver(s: &str) -> Option<ComposerVersion> {
    ComposerVersion::parse(s.trim().trim_start_matches('v')).ok()
}

/// Compare two version strings after Composer-style parsing (avoids `v1.0.0` ≠ `1.0.0`).
pub fn versions_equal(a: &str, b: &str) -> bool {
    match (parse_ver(a), parse_ver(b)) {
        (Some(va), Some(vb)) => {
            va == vb
                || (va.numeric_parts() == vb.numeric_parts()
                    && va.stability() == vb.stability()
                    && va.normalized() == vb.normalized())
        }
        _ => a.trim_start_matches('v') == b.trim_start_matches('v'),
    }
}

fn version_less(a: &str, b: &str) -> bool {
    match (parse_ver(a), parse_ver(b)) {
        (Some(va), Some(vb)) => va < vb,
        _ => a.trim_start_matches('v') < b.trim_start_matches('v'),
    }
}

/// Classify Composer outdated status from current / wanted / latest strings.
pub fn classify_status(current: &str, wanted: &str, latest: &str) -> OutdatedKind {
    if versions_equal(current, latest) {
        return OutdatedKind::UpToDate;
    }
    // Semver-safe: a higher version is allowed by the constraint (wanted > current)
    if version_less(current, wanted) {
        return OutdatedKind::SemverSafe;
    }
    // Only major (or otherwise unconstrained) newer release beyond wanted
    if version_less(wanted, latest) || version_less(current, latest) {
        return OutdatedKind::MajorOnly;
    }
    OutdatedKind::UpToDate
}

/// Compute Current / Wanted / Latest for one locked package given registry versions.
pub fn compute_row(
    pkg: &LockedPackage,
    versions: &[RemotePackageVersion],
    root_constraint: Option<&str>,
) -> OutdatedRow {
    let constraint = root_constraint.map(|c| VersionConstraint::new(c.to_string()));
    let current = pkg.version.clone();
    let mut latest: Option<ComposerVersion> = None;
    let mut wanted: Option<ComposerVersion> = None;
    for v in versions {
        if !v.version.is_stable() {
            continue;
        }
        if latest.as_ref().is_none_or(|l| v.version > *l) {
            latest = Some(v.version.clone());
        }
        if let Some(c) = &constraint {
            if c.matches(&v.version) && wanted.as_ref().is_none_or(|w| v.version > *w) {
                wanted = Some(v.version.clone());
            }
        }
    }
    // Transitive: no root constraint → wanted tracks latest
    if constraint.is_none() {
        wanted = latest.clone();
    }

    // Prefer pretty raw strings from selected versions; fall back to lock current
    let latest_s = latest
        .as_ref()
        .map(|v| v.raw.clone())
        .unwrap_or_else(|| current.clone());
    let wanted_s = wanted
        .as_ref()
        .map(|v| v.raw.clone())
        .unwrap_or_else(|| current.clone());

    let status = classify_status(&current, &wanted_s, &latest_s);

    OutdatedRow {
        name: pkg.name.clone(),
        current,
        wanted: wanted_s,
        latest: latest_s,
        status,
        description: pkg.description.clone(),
    }
}

fn matches_filter(row: &OutdatedRow, major_only: bool, minor_only: bool) -> bool {
    if !row.is_outdated() {
        return true; // filtering applies to outdated listing; up-to-date handled by --all
    }
    if major_only && minor_only {
        return true;
    }
    if major_only {
        return row.status == OutdatedKind::MajorOnly;
    }
    if minor_only {
        return row.status == OutdatedKind::SemverSafe;
    }
    true
}

pub async fn run(args: OutdatedArgs) -> Result<()> {
    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    if !lock_path.exists() {
        bail!("composer.lock not found — run install/update first");
    }
    let manifest = ComposerJson::load(&json_path)?;
    let lock = ComposerLock::load(&lock_path)?;
    let with_dev = !args.no_dev;
    let registry = RepositoryRegistry::from_manifest_auth(
        &manifest,
        composer_auth::AuthStore::load(Some(&cwd)).unwrap_or_default(),
    )?;

    let root_names: std::collections::BTreeSet<String> = {
        let mut s = std::collections::BTreeSet::new();
        for k in manifest.require.keys() {
            if PackageId::parse(k).is_ok_and(|p| !p.is_platform()) {
                s.insert(k.clone());
            }
        }
        if with_dev {
            for k in manifest.require_dev.keys() {
                if PackageId::parse(k).is_ok_and(|p| !p.is_platform()) {
                    s.insert(k.clone());
                }
            }
        }
        s
    };

    let packages = lock.packages_to_install(with_dev);
    let mut rows = Vec::new();
    let mut fetch_failures = 0usize;
    let mut checked = 0usize;

    for pkg in packages {
        if args.direct && !root_names.contains(&pkg.name) {
            continue;
        }
        let id = PackageId::parse(&pkg.name)?;
        let versions = match registry.get_package_versions(&id).await {
            Ok(v) => v,
            Err(e) => {
                warning(&format!("{}: could not fetch metadata ({e})", pkg.name));
                fetch_failures += 1;
                continue;
            }
        };
        checked += 1;
        let constraint = manifest
            .require
            .get(&pkg.name)
            .or_else(|| manifest.require_dev.get(&pkg.name))
            .map(|s| s.as_str());

        let row = compute_row(pkg, &versions, constraint);
        if !matches_filter(&row, args.major_only, args.minor_only) {
            continue;
        }
        if !row.is_outdated() && !args.all {
            continue;
        }
        // With --major-only / --minor-only, hide up-to-date even with --all
        if !row.is_outdated() && (args.major_only || args.minor_only) {
            continue;
        }
        rows.push(row);
    }

    if args.format == "json" {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() && fetch_failures == 0 {
        info("All packages are up to date");
    } else if rows.is_empty() {
        warning("Could not verify every package; not treating the lock as up to date");
    } else {
        println!(
            "{:<3} {:<40} {:<16} {:<16} {:<16}",
            "", "Package", "Current", "Wanted", "Latest"
        );
        for r in &rows {
            println!(
                "{:<3} {:<40} {:<16} {:<16} {:<16}",
                r.status.marker(),
                r.name,
                r.current,
                r.wanted,
                r.latest
            );
        }
        info("! = semver-safe update available; ~ = major update available; = = up to date");
    }

    // Composer: --strict fails when any package is outdated (semver-safe or major).
    let outdated_count = rows.iter().filter(|r| r.is_outdated()).count();
    if args.strict && fetch_failures > 0 {
        bail!("{fetch_failures} package(s) could not be checked (--strict); checked {checked}");
    }
    if args.strict && outdated_count > 0 {
        bail!("{outdated_count} outdated package(s) (--strict)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use composer_core::ComposerVersion;
    use composer_lock::DistInfo;
    use std::collections::BTreeMap;

    fn locked(name: &str, ver: &str) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: ver.into(),
            source: None,
            dist: Some(DistInfo {
                dist_type: "zip".into(),
                url: format!("https://example.com/{name}.zip"),
                reference: None,
                shasum: None,
                mirrors: None,
            }),
            require: BTreeMap::new(),
            require_dev: BTreeMap::new(),
            package_type: Some("library".into()),
            extra: None,
            autoload: None,
            autoload_dev: None,
            notification_url: None,
            license: vec![],
            description: None,
            homepage: None,
            keywords: vec![],
            time: None,
            replace: BTreeMap::new(),
            provide: BTreeMap::new(),
            conflict: BTreeMap::new(),
            suggest: BTreeMap::new(),
            bin: vec![],
            abandoned: None,
            unknown: BTreeMap::new(),
        }
    }

    fn remote(name: &str, ver: &str) -> RemotePackageVersion {
        RemotePackageVersion {
            name: name.into(),
            version: ComposerVersion::parse(ver).unwrap(),
            version_normalized: format!("{ver}.0"),
            dist: None,
            source: None,
            require: BTreeMap::new(),
            require_dev: BTreeMap::new(),
            package_type: Some("library".into()),
            autoload: None,
            autoload_dev: None,
            provide: BTreeMap::new(),
            replace: BTreeMap::new(),
            conflict: BTreeMap::new(),
            bin: vec![],
            description: None,
            license: vec![],
            abandoned: None,
            time: None,
        }
    }

    #[test]
    fn wanted_respects_constraint_latest_is_max_stable() {
        let pkg = locked("acme/foo", "1.0.0");
        let versions = vec![
            remote("acme/foo", "1.0.0"),
            remote("acme/foo", "1.5.0"),
            remote("acme/foo", "2.0.0"),
        ];
        let row = compute_row(&pkg, &versions, Some("^1.0"));
        assert_eq!(row.current, "1.0.0");
        assert_eq!(row.wanted, "1.5.0");
        assert_eq!(row.latest, "2.0.0");
        assert_eq!(row.status, OutdatedKind::SemverSafe);
        assert!(row.is_outdated());
    }

    #[test]
    fn major_only_when_at_wanted_but_latest_is_major() {
        let pkg = locked("acme/foo", "1.5.0");
        let versions = vec![remote("acme/foo", "1.5.0"), remote("acme/foo", "2.0.0")];
        let row = compute_row(&pkg, &versions, Some("^1.0"));
        assert_eq!(row.status, OutdatedKind::MajorOnly);
        assert!(row.is_outdated());
    }

    #[test]
    fn up_to_date_when_current_equals_latest() {
        let pkg = locked("acme/foo", "2.0.0");
        let versions = vec![remote("acme/foo", "2.0.0")];
        let row = compute_row(&pkg, &versions, Some("^2.0"));
        assert_eq!(row.status, OutdatedKind::UpToDate);
        assert!(!row.is_outdated());
    }

    #[test]
    fn version_equality_ignores_v_prefix() {
        assert!(versions_equal("v1.0.0", "1.0.0"));
        assert!(versions_equal("1.2.3", "1.2.3"));
        assert!(!versions_equal("1.0.0", "1.0.1"));
    }

    #[test]
    fn classify_strict_triggers_on_major_and_semver_safe() {
        assert!(classify_status("1.0.0", "1.5.0", "2.0.0").is_outdated());
        assert!(classify_status("1.5.0", "1.5.0", "2.0.0").is_outdated());
        assert!(!classify_status("2.0.0", "2.0.0", "2.0.0").is_outdated());
    }
}
