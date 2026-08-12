//! Package identifiers and autoload metadata.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Package identifier in `vendor/name` form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageId(String);

impl PackageId {
    pub fn new(vendor: impl Into<String>, name: impl Into<String>) -> Self {
        Self(format!("{}/{}", vendor.into(), name.into()))
    }

    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        // Platform packages (php, ext-*, lib-*, composer-plugin-api, ...) have no slash.
        if !s.contains('/') {
            if s.is_empty() {
                return Err(Error::InvalidPackageName(s.to_string()));
            }
            return Ok(Self(s.to_string()));
        }
        let (vendor, name) = s.split_once('/').unwrap();
        if vendor.is_empty() || name.is_empty() {
            return Err(Error::InvalidPackageName(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn vendor(&self) -> &str {
        self.0.split_once('/').map(|(v, _)| v).unwrap_or(&self.0)
    }

    pub fn name(&self) -> &str {
        self.0.split_once('/').map(|(_, n)| n).unwrap_or(&self.0)
    }

    /// Platform packages (`php`, `ext-*`, …) never use the `vendor/name` form.
    pub fn is_platform(&self) -> bool {
        !self.0.contains('/')
    }

    /// Vendor-relative install path: `vendor/name`.
    pub fn install_path(&self) -> String {
        self.0.clone()
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PackageId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl AsRef<str> for PackageId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Composer package type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PackageType {
    #[default]
    Library,
    Project,
    Metapackage,
    ComposerPlugin,
    #[serde(other)]
    Other,
}

/// Autoload configuration (PSR-4, PSR-0, classmap, files).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoloadConfig {
    #[serde(rename = "psr-4", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub psr4: BTreeMap<String, PathOrPaths>,

    #[serde(rename = "psr-0", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub psr0: BTreeMap<String, PathOrPaths>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classmap: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_from_classmap: Vec<String>,
}

/// A single path or list of paths (Composer allows both).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PathOrPaths {
    One(String),
    Many(Vec<String>),
}

impl PathOrPaths {
    pub fn paths(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normal() {
        let id = PackageId::parse("symfony/console").unwrap();
        assert_eq!(id.vendor(), "symfony");
        assert_eq!(id.name(), "console");
        assert!(!id.is_platform());
    }

    #[test]
    fn parse_platform() {
        let id = PackageId::parse("php").unwrap();
        assert!(id.is_platform());
        let ext = PackageId::parse("ext-json").unwrap();
        assert!(ext.is_platform());
    }

    #[test]
    fn vendor_packages_are_not_platform() {
        assert!(!PackageId::parse("phpunit/phpunit").unwrap().is_platform());
        assert!(!PackageId::parse("php/foo").unwrap().is_platform());
    }
}
