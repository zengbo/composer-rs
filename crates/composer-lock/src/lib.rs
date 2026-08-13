//! Composer-compatible lockfile types and I/O.

#![deny(unsafe_code)]

use composer_core::error::{Error, Result};
use composer_core::{AutoloadConfig, PackageId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Complete `composer.lock` document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposerLock {
    #[serde(rename = "_readme", default)]
    pub readme: Vec<String>,

    #[serde(rename = "content-hash", default)]
    pub content_hash: String,

    #[serde(default)]
    pub packages: Vec<LockedPackage>,

    #[serde(rename = "packages-dev", default)]
    pub packages_dev: Vec<LockedPackage>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<serde_json::Value>,

    #[serde(rename = "minimum-stability", default = "default_stability")]
    pub minimum_stability: String,

    #[serde(rename = "stability-flags", default)]
    pub stability_flags: BTreeMap<String, i32>,

    #[serde(rename = "prefer-stable", default)]
    pub prefer_stable: bool,

    #[serde(rename = "prefer-lowest", default)]
    pub prefer_lowest: bool,

    #[serde(default)]
    pub platform: BTreeMap<String, String>,

    #[serde(rename = "platform-dev", default)]
    pub platform_dev: BTreeMap<String, String>,

    #[serde(rename = "plugin-api-version", default = "default_plugin_api")]
    pub plugin_api_version: String,
}

fn default_stability() -> String {
    "stable".into()
}

fn default_plugin_api() -> String {
    "2.6.0".into()
}

impl Default for ComposerLock {
    fn default() -> Self {
        Self {
            readme: vec![
                "This file locks the dependencies of your project to a known state".into(),
                "Read more about it at https://getcomposer.org/doc/01-basic-usage.md#installing-dependencies".into(),
                "This file is @generated automatically".into(),
            ],
            content_hash: String::new(),
            packages: Vec::new(),
            packages_dev: Vec::new(),
            aliases: Vec::new(),
            minimum_stability: default_stability(),
            stability_flags: BTreeMap::new(),
            prefer_stable: false,
            prefer_lowest: false,
            platform: BTreeMap::new(),
            platform_dev: BTreeMap::new(),
            plugin_api_version: default_plugin_api(),
        }
    }
}

impl ComposerLock {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::from_str(&text)
    }

    pub fn from_str(text: &str) -> Result<Self> {
        serde_json::from_str(text).map_err(|e| Error::Lockfile(e.to_string()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        // Composer lock ends with newline
        let mut out = json;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        fs::write(path, out).map_err(|e| Error::io(path, e))
    }

    /// All packages to install (prod + optionally dev).
    pub fn packages_to_install(&self, with_dev: bool) -> Vec<&LockedPackage> {
        let mut pkgs: Vec<&LockedPackage> = self.packages.iter().collect();
        if with_dev {
            pkgs.extend(self.packages_dev.iter());
        }
        pkgs
    }

    pub fn find(&self, name: &str) -> Option<&LockedPackage> {
        self.packages
            .iter()
            .chain(self.packages_dev.iter())
            .find(|p| p.name == name)
    }

    /// Rebuild a lock-shaped document from Composer 1/2 `installed.json`.
    ///
    /// Official `dump-autoload` uses this file when `composer.lock` is absent.
    pub fn load_installed_json(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::from_installed_json_str(&text)
    }

    pub fn from_installed_json_str(text: &str) -> Result<Self> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| Error::Lockfile(e.to_string()))?;
        let (raw_packages, dev_names) = match value {
            serde_json::Value::Array(arr) => (arr, Vec::new()),
            serde_json::Value::Object(obj) => {
                let pkgs = obj
                    .get("packages")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let names = obj
                    .get("dev-package-names")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                (pkgs, names)
            }
            _ => {
                return Err(Error::Lockfile(
                    "installed.json must be an array or a Composer 2 object".into(),
                ));
            }
        };

        let dev: std::collections::BTreeSet<String> = dev_names.into_iter().collect();
        let mut packages = Vec::new();
        let mut packages_dev = Vec::new();
        for (i, raw) in raw_packages.into_iter().enumerate() {
            let pkg = locked_from_installed_entry(raw)
                .map_err(|e| Error::Lockfile(format!("installed.json package #{i}: {e}")))?;
            if dev.contains(&pkg.name) {
                packages_dev.push(pkg);
            } else {
                packages.push(pkg);
            }
        }

        Ok(Self {
            packages,
            packages_dev,
            ..Self::default()
        })
    }
}

fn locked_from_installed_entry(mut value: serde_json::Value) -> Result<LockedPackage> {
    if let Some(obj) = value.as_object_mut() {
        obj.remove("version_normalized");
        obj.remove("install-path");
        wrap_string_as_array(obj, "license");
        wrap_string_as_array(obj, "bin");
        wrap_string_as_array(obj, "keywords");
    }
    serde_json::from_value(value).map_err(|e| Error::Lockfile(e.to_string()))
}

fn wrap_string_as_array(obj: &mut serde_json::Map<String, serde_json::Value>, key: &str) {
    let Some(val) = obj.get(key) else {
        return;
    };
    if val.is_string() {
        let s = val.clone();
        obj.insert(key.to_string(), serde_json::Value::Array(vec![s]));
    }
}

/// A locked package entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceInfo>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dist: Option<DistInfo>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub require: BTreeMap<String, String>,

    #[serde(
        rename = "require-dev",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub require_dev: BTreeMap<String, String>,

    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub package_type: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoload: Option<AutoloadConfig>,

    #[serde(
        rename = "autoload-dev",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub autoload_dev: Option<AutoloadConfig>,

    #[serde(
        rename = "notification-url",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub notification_url: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub license: Vec<License>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,

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

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned: Option<serde_json::Value>,
}

impl LockedPackage {
    pub fn package_id(&self) -> Result<PackageId> {
        PackageId::parse(&self.name)
    }

    /// Cache key for CAS: prefer dist shasum, else URL+reference.
    pub fn cache_key(&self) -> String {
        if let Some(dist) = &self.dist {
            if let Some(shasum) = &dist.shasum {
                if !shasum.is_empty() {
                    return format!("sha1:{}", shasum);
                }
            }
            if let Some(ref_) = &dist.reference {
                return format!("dist:{}@{}", dist.url, ref_);
            }
            return format!("dist:{}", dist.url);
        }
        if let Some(source) = &self.source {
            return format!(
                "src:{}@{}",
                source.url,
                source.reference.as_deref().unwrap_or("")
            );
        }
        format!("{}@{}", self.name, self.version)
    }

    pub fn dist_url(&self) -> Option<&str> {
        self.dist.as_ref().map(|d| d.url.as_str())
    }

    pub fn is_metapackage(&self) -> bool {
        self.package_type.as_deref() == Some("metapackage")
            || self.dist.is_none() && self.source.is_none()
    }

    /// Whether a path-repository package should be symlinked into vendor (default: true).
    pub fn path_symlink(&self) -> bool {
        if let Some(extra) = &self.extra {
            if let Some(v) = extra
                .get("composer-rs")
                .and_then(|v| v.get("symlink"))
                .and_then(|v| v.as_bool())
            {
                return v;
            }
        }
        true
    }
}

/// Dist archive info.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DistInfo {
    #[serde(rename = "type")]
    pub dist_type: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shasum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirrors: Option<Vec<serde_json::Value>>,
}

/// VCS source info.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceInfo {
    #[serde(rename = "type")]
    pub source_type: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// License as string or array.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum License {
    One(String),
    Many(Vec<String>),
}

/// Compute Composer-compatible content-hash from raw `composer.json` bytes.
pub fn content_hash_from_composer_json(
    composer_json_bytes: &[u8],
) -> composer_core::Result<String> {
    composer_manifest::content_hash(composer_json_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_minimal() {
        let lock = ComposerLock {
            packages: vec![LockedPackage {
                name: "foo/bar".into(),
                version: "1.0.0".into(),
                source: None,
                dist: Some(DistInfo {
                    dist_type: "zip".into(),
                    url: "https://example.com/foo.zip".into(),
                    reference: Some("abc".into()),
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
            }],
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&lock).unwrap();
        let parsed = ComposerLock::from_str(&json).unwrap();
        assert_eq!(parsed.packages[0].name, "foo/bar");
    }

    #[test]
    fn path_symlink_from_extra() {
        let mut pkg = LockedPackage {
            name: "acme/lib".into(),
            version: "1.0.0".into(),
            source: None,
            dist: Some(DistInfo {
                dist_type: "path".into(),
                url: "/tmp/lib".into(),
                reference: None,
                shasum: None,
                mirrors: None,
            }),
            require: BTreeMap::new(),
            require_dev: BTreeMap::new(),
            package_type: Some("library".into()),
            extra: Some(serde_json::json!({ "composer-rs": { "symlink": false } })),
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
        };
        assert!(!pkg.path_symlink());
        pkg.extra = None;
        assert!(pkg.path_symlink());
    }

    #[test]
    fn content_hash_from_composer_json_is_md5_hex() {
        let json = br#"{"require":{"symfony/console":"^6.0"}}"#;
        let hash = content_hash_from_composer_json(json).unwrap();
        assert_eq!(hash.len(), 32);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn installed_json_splits_dev_packages() {
        let json = r#"{
            "packages": [
                {
                    "name": "acme/lib",
                    "version": "1.0.0",
                    "type": "library",
                    "license": "MIT",
                    "install-path": "../acme/lib",
                    "version_normalized": "1.0.0.0"
                },
                {
                    "name": "phpunit/phpunit",
                    "version": "10.0.0",
                    "type": "library"
                }
            ],
            "dev": true,
            "dev-package-names": ["phpunit/phpunit"]
        }"#;
        let lock = ComposerLock::from_installed_json_str(json).unwrap();
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "acme/lib");
        assert_eq!(lock.packages_dev.len(), 1);
        assert_eq!(lock.packages_dev[0].name, "phpunit/phpunit");
        assert_eq!(lock.packages_to_install(false).len(), 1);
        assert_eq!(lock.packages_to_install(true).len(), 2);
    }
}
