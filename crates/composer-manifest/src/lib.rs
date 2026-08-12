//! `composer.json` manifest parsing.

#![deny(unsafe_code)]

pub mod installer_paths;
pub mod repositories;

pub use installer_paths::InstallerPaths;
pub use repositories::{
    parse_repositories, resolve_path_url, PathPackageManifest, Repository,
};

use composer_core::error::{Error, Result};
use composer_core::{AutoloadConfig, PackageId, VersionConstraint};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Root project `composer.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComposerJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub package_type: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<Value>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub require: BTreeMap<String, String>,

    #[serde(
        rename = "require-dev",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub require_dev: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoload: Option<AutoloadConfig>,

    #[serde(
        rename = "autoload-dev",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub autoload_dev: Option<AutoloadConfig>,

    #[serde(
        rename = "minimum-stability",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub minimum_stability: Option<String>,

    #[serde(
        rename = "prefer-stable",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub prefer_stable: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts: Option<BTreeMap<String, Value>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repositories: Option<Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub replace: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provide: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub conflict: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub suggest: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bin: Vec<String>,

    /// Preserve unknown fields for round-trip safety.
    #[serde(flatten)]
    pub rest: BTreeMap<String, Value>,
}

impl ComposerJson {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::from_str(&text)
    }

    pub fn from_str(text: &str) -> Result<Self> {
        serde_json::from_str(text).map_err(|e| Error::Manifest(e.to_string()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        let mut out = json;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        fs::write(path, out).map_err(|e| Error::io(path, e))
    }

    /// Non-platform production dependencies.
    pub fn prod_deps(&self) -> Result<Vec<(PackageId, VersionConstraint)>> {
        parse_deps(&self.require)
    }

    /// Non-platform dev dependencies.
    pub fn dev_deps(&self) -> Result<Vec<(PackageId, VersionConstraint)>> {
        parse_deps(&self.require_dev)
    }

    pub fn all_deps(&self, with_dev: bool) -> Result<Vec<(PackageId, VersionConstraint)>> {
        let mut deps = self.prod_deps()?;
        if with_dev {
            deps.extend(self.dev_deps()?);
        }
        Ok(deps)
    }

    /// Vendor directory name from config (default `vendor`).
    pub fn vendor_dir(&self) -> String {
        self.config
            .as_ref()
            .and_then(|c| c.get("vendor-dir"))
            .and_then(|v| v.as_str())
            .unwrap_or("vendor")
            .to_string()
    }

    pub fn prefer_stable(&self) -> bool {
        self.prefer_stable.unwrap_or(false)
    }

    pub fn minimum_stability(&self) -> &str {
        self.minimum_stability.as_deref().unwrap_or("stable")
    }

    /// Parsed custom repositories (path / vcs / composer).
    pub fn repositories_list(&self) -> Vec<Repository> {
        parse_repositories(self.repositories.as_ref())
    }

    /// Whether Packagist is enabled (default true unless `packagist.org: false`).
    pub fn packagist_enabled(&self) -> bool {
        let Some(value) = &self.repositories else {
            return true;
        };
        let Value::Object(map) = value else {
            return true;
        };
        for (key, val) in map {
            if (key == "packagist.org" || key == "packagist") && val.as_bool() == Some(false) {
                return false;
            }
        }
        true
    }

    /// Custom installer-paths from `extra`.
    pub fn installer_paths(&self) -> InstallerPaths {
        InstallerPaths::from_extra(self.extra.as_ref())
    }
}

fn parse_deps(map: &BTreeMap<String, String>) -> Result<Vec<(PackageId, VersionConstraint)>> {
    let mut out = Vec::new();
    for (name, constraint) in map {
        let id = PackageId::parse(name)?;
        if id.is_platform() {
            continue;
        }
        out.push((id, VersionConstraint::new(constraint.clone())));
    }
    Ok(out)
}

/// JSON fragment used for Composer content-hash.
///
/// Mirrors Composer\Package\Locker::getContentHash relevant keys and encodes
/// compact JSON with unsorted nested objects represented via BTreeMap (sorted
/// keys) — close enough for drift detection; exact byte match with every
/// Composer version is not guaranteed for exotic key order.
pub fn relevant_content(manifest: &ComposerJson) -> String {
    // Rebuild from a value map so we only include present keys (Composer style).
    let mut relevant = BTreeMap::new();

    if let Some(name) = &manifest.name {
        relevant.insert("name".into(), Value::String(name.clone()));
    }
    if let Some(version) = &manifest.version {
        relevant.insert("version".into(), Value::String(version.clone()));
    }
    if !manifest.require.is_empty() {
        relevant.insert("require".into(), map_to_object(&manifest.require));
    }
    if !manifest.require_dev.is_empty() {
        relevant.insert("require-dev".into(), map_to_object(&manifest.require_dev));
    }
    if !manifest.conflict.is_empty() {
        relevant.insert("conflict".into(), map_to_object(&manifest.conflict));
    }
    if !manifest.replace.is_empty() {
        relevant.insert("replace".into(), map_to_object(&manifest.replace));
    }
    if !manifest.provide.is_empty() {
        relevant.insert("provide".into(), map_to_object(&manifest.provide));
    }
    if let Some(ms) = &manifest.minimum_stability {
        relevant.insert("minimum-stability".into(), Value::String(ms.clone()));
    }
    if let Some(ps) = manifest.prefer_stable {
        relevant.insert("prefer-stable".into(), Value::Bool(ps));
    }
    if let Some(repos) = &manifest.repositories {
        relevant.insert("repositories".into(), repos.clone());
    }
    if let Some(extra) = &manifest.extra {
        relevant.insert("extra".into(), extra.clone());
    }
    // config.platform only (Composer includes just this slice of config)
    if let Some(platform) = manifest
        .config
        .as_ref()
        .and_then(|c| c.get("platform"))
        .cloned()
    {
        let mut config = serde_json::Map::new();
        config.insert("platform".into(), platform);
        relevant.insert("config".into(), Value::Object(config));
    }

    // Compact JSON, sorted top-level keys via BTreeMap, unescaped slashes.
    let value = Value::Object(relevant.into_iter().collect());
    // serde_json compact encode; replace escaped slashes for closer Composer match.
    serde_json::to_string(&value)
        .unwrap_or_default()
        .replace("\\/", "/")
}

fn map_to_object(map: &BTreeMap<String, String>) -> Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in map {
        obj.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let json = r#"{
            "name": "acme/app",
            "require": {
                "php": ">=8.1",
                "symfony/console": "^6.0"
            }
        }"#;
        let m = ComposerJson::from_str(json).unwrap();
        assert_eq!(m.name.as_deref(), Some("acme/app"));
        let deps = m.prod_deps().unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].0.as_str(), "symfony/console");
    }

    #[test]
    fn relevant_content_includes_stability() {
        let m = ComposerJson {
            name: Some("acme/app".into()),
            require: BTreeMap::from([("php".into(), ">=8.1".into())]),
            minimum_stability: Some("dev".into()),
            prefer_stable: Some(true),
            ..Default::default()
        };
        let s = relevant_content(&m);
        assert!(s.contains("minimum-stability"));
        assert!(s.contains("prefer-stable"));
        assert!(!s.contains("\\/"));
    }
}
