//! `composer.json` manifest parsing.

#![deny(unsafe_code)]

pub mod installer_paths;
pub mod repositories;

pub mod content_hash;

pub use content_hash::content_hash;
pub use installer_paths::InstallerPaths;
pub use repositories::{PathPackageManifest, Repository, parse_repositories, resolve_path_url};

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

    /// Binary directory relative to project root (default `vendor/bin`).
    pub fn bin_dir(&self) -> String {
        self.config
            .as_ref()
            .and_then(|c| c.get("bin-dir"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}/bin", self.vendor_dir()))
    }

    /// Whether to generate/run `platform_check.php` (`config.platform-check`).
    ///
    /// Composer: `true` (default) / `false` / `"php-only"`.
    pub fn platform_check_enabled(&self) -> bool {
        match self.config.as_ref().and_then(|c| c.get("platform-check")) {
            Some(serde_json::Value::Bool(false)) => false,
            Some(serde_json::Value::String(s)) if s.eq_ignore_ascii_case("false") => false,
            _ => true,
        }
    }

    /// Preferred install method from config: `"dist"`, `"source"`, or `"auto"`.
    pub fn preferred_install(&self) -> PreferInstall {
        match self
            .config
            .as_ref()
            .and_then(|c| {
                c.get("preferred-install")
                    .or_else(|| c.get("prefer-install"))
            })
            .and_then(|v| v.as_str())
        {
            Some("source") => PreferInstall::Source,
            Some("dist") => PreferInstall::Dist,
            _ => PreferInstall::Auto,
        }
    }

    /// Resolve whether to prefer dist archives given CLI flags and config.
    pub fn resolve_prefer_dist(&self, prefer_dist_flag: bool, prefer_source_flag: bool) -> bool {
        if prefer_source_flag {
            return false;
        }
        if !prefer_dist_flag {
            // explicit --prefer-dist=false style (rare)
            return matches!(self.preferred_install(), PreferInstall::Dist);
        }
        !matches!(self.preferred_install(), PreferInstall::Source)
    }

    /// `config.allow-plugins` map (package name or `*` → allowed).
    pub fn allow_plugins(&self) -> BTreeMap<String, bool> {
        let mut out = BTreeMap::new();
        let Some(map) = self
            .config
            .as_ref()
            .and_then(|c| c.get("allow-plugins"))
            .and_then(|v| v.as_object())
        else {
            return out;
        };
        for (k, v) in map {
            if let Some(b) = v.as_bool() {
                out.insert(k.clone(), b);
            }
        }
        out
    }

    /// Get a nested config string value by dotted key (e.g. `platform.php`).
    pub fn config_get(&self, key: &str) -> Option<Value> {
        let mut cur = self.config.as_ref()?;
        for part in key.split('.') {
            cur = cur.get(part)?;
        }
        Some(cur.clone())
    }

    /// Set a nested config value by dotted key, creating objects as needed.
    pub fn config_set(&mut self, key: &str, value: Value) {
        let parts: Vec<&str> = key.split('.').collect();
        if parts.is_empty() {
            return;
        }
        let mut root = self
            .config
            .take()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if !root.is_object() {
            root = Value::Object(serde_json::Map::new());
        }
        {
            let mut map = root.as_object_mut().expect("object");
            for (i, part) in parts.iter().enumerate() {
                if i + 1 == parts.len() {
                    map.insert((*part).to_string(), value.clone());
                } else {
                    let entry = map
                        .entry((*part).to_string())
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if !entry.is_object() {
                        *entry = Value::Object(serde_json::Map::new());
                    }
                    map = entry.as_object_mut().expect("object");
                }
            }
        }
        self.config = Some(root);
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

    /// Composer `config.secure-http` (default true): reject plaintext HTTP remotes.
    pub fn secure_http(&self) -> bool {
        self.config
            .as_ref()
            .and_then(|c| c.get("secure-http"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
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

/// `config.preferred-install` / `prefer-install`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreferInstall {
    #[default]
    Auto,
    Dist,
    Source,
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
