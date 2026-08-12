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

/// JSON fragment used for content-hash (relevant dependency fields).
pub fn relevant_content(manifest: &ComposerJson) -> String {
    let mut map = BTreeMap::new();
    if !manifest.require.is_empty() {
        map.insert("require", &manifest.require);
    }
    // serialize sorted
    let mut obj = serde_json::Map::new();
    for (k, v) in &manifest.require {
        obj.insert(k.clone(), Value::String(v.clone()));
    }
    let require = Value::Object(obj);
    let mut root = serde_json::Map::new();
    root.insert("require".into(), require);
    if !manifest.require_dev.is_empty() {
        let mut dev = serde_json::Map::new();
        for (k, v) in &manifest.require_dev {
            dev.insert(k.clone(), Value::String(v.clone()));
        }
        root.insert("require-dev".into(), Value::Object(dev));
    }
    serde_json::to_string(&Value::Object(root)).unwrap_or_default()
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
}
