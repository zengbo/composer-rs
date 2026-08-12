//! Runtime platform detection and requirement checking (`php`, `ext-*`, …).

use crate::AHashSet;
use crate::error::{Error, Result};
use crate::version::{ComposerVersion, VersionConstraint};
use serde_json::Value;
use std::process::Command;

/// Detected PHP platform (version + loaded extensions).
#[derive(Debug, Clone)]
pub struct Platform {
    pub php: ComposerVersion,
    extensions: AHashSet<String>,
    /// True when PHP was probed successfully or overridden via env/`config.platform`.
    /// When false, platform checks should not pretend requirements are satisfied.
    pub reliable: bool,
}

impl Platform {
    /// Detect from `php` on PATH, with optional overrides via env vars.
    ///
    /// - `COMPOSER_PLATFORM_PHP` — force PHP version string (e.g. `8.2.0`)
    /// - `COMPOSER_PLATFORM_EXT_<NAME>` — mark extension as available (`1`)
    ///
    /// If PHP is missing and no override is set, returns `reliable: false` with a
    /// placeholder PHP version. Callers must not treat requirements as met.
    pub fn detect() -> Result<Self> {
        let mut reliable = false;

        let php = if let Ok(v) = std::env::var("COMPOSER_PLATFORM_PHP") {
            reliable = true;
            ComposerVersion::parse(&v)?
        } else if let Some(v) = detect_php_version() {
            reliable = true;
            v
        } else {
            // Placeholder only — must not be used to green-light constraints.
            ComposerVersion::parse("0.0.0").expect("0.0.0 parses")
        };

        let mut extensions = if reliable {
            detect_php_extensions().unwrap_or_default()
        } else {
            AHashSet::new()
        };

        for (key, val) in std::env::vars() {
            if let Some(ext) = key.strip_prefix("COMPOSER_PLATFORM_EXT_") {
                if val == "1" || val.eq_ignore_ascii_case("true") {
                    extensions.insert(ext.to_ascii_lowercase().replace('_', "-"));
                    reliable = true;
                }
            }
        }

        Ok(Self {
            php,
            extensions,
            reliable,
        })
    }

    /// Apply `config.platform` overrides from composer.json (Composer-compatible).
    pub fn apply_config_platform(&mut self, config: Option<&Value>) {
        let Some(platform) = config
            .and_then(|c| c.get("platform"))
            .and_then(|p| p.as_object())
        else {
            return;
        };

        for (name, val) in platform {
            let version = match val {
                Value::String(s) => s.as_str(),
                Value::Bool(false) => continue,
                Value::Number(n) => {
                    // uncommon but allow
                    let owned = n.to_string();
                    if name == "php" || name == "hhvm" {
                        if let Ok(v) = ComposerVersion::parse(&owned) {
                            self.php = v;
                            self.reliable = true;
                        }
                    }
                    continue;
                }
                _ => continue,
            };

            if name == "php" || name == "hhvm" {
                if let Ok(v) = ComposerVersion::parse(version) {
                    self.php = v;
                    self.reliable = true;
                }
            } else if let Some(ext) = name.strip_prefix("ext-") {
                // Any non-false value means the extension is available.
                if val.as_bool() != Some(false) {
                    self.extensions.insert(ext.to_ascii_lowercase());
                    self.reliable = true;
                }
            }
        }
    }

    /// Whether a platform package requirement is satisfied.
    pub fn satisfies(&self, name: &str, constraint: &str) -> bool {
        if !self.reliable {
            return false;
        }
        if name == "php" || name == "hhvm" {
            return VersionConstraint::new(constraint).matches(&self.php);
        }
        if let Some(ext) = name.strip_prefix("ext-") {
            return self.extension_loaded(ext, constraint);
        }
        if name.starts_with("lib-") || name.starts_with("composer-") {
            // Composer treats lib/composer API checks separately; assume OK when reliable.
            return true;
        }
        // Unknown single-segment platform identifiers: only match against PHP version.
        if !name.contains('/') {
            return VersionConstraint::new(constraint).matches(&self.php);
        }
        true
    }

    fn extension_loaded(&self, ext: &str, constraint: &str) -> bool {
        let normalized = ext.to_ascii_lowercase();
        if !self.extensions.contains(&normalized) {
            return false;
        }
        // Composer rarely pins ext versions; `*` / `1` mean "loaded".
        let _ = constraint;
        true
    }

    pub fn php_version(&self) -> &ComposerVersion {
        &self.php
    }

    /// Build a platform snapshot for tests or overrides (always reliable).
    pub fn with_php(php: &str) -> Result<Self> {
        Ok(Self {
            php: ComposerVersion::parse(php)?,
            extensions: AHashSet::new(),
            reliable: true,
        })
    }

    pub fn with_php_and_extensions(
        php: &str,
        extensions: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self> {
        Ok(Self {
            php: ComposerVersion::parse(php)?,
            extensions: extensions
                .into_iter()
                .map(|e| e.as_ref().to_ascii_lowercase())
                .collect(),
            reliable: true,
        })
    }
}

/// Whether a platform package name matches an `--ignore-platform-req` pattern.
///
/// Patterns may be exact (`ext-xdebug`, `php`) or end with `*` (`ext-*`, `php*`).
/// A trailing `+` (Composer upper-bound ignore) is accepted but treated as full ignore.
pub fn platform_req_ignored(name: &str, patterns: &[String]) -> bool {
    for pat in patterns {
        let pat = pat.trim_end_matches('+');
        if pat == name {
            return true;
        }
        if let Some(prefix) = pat.strip_suffix('*') {
            if name.starts_with(prefix) {
                return true;
            }
        }
    }
    false
}

/// Check all platform requirements on a dependency map.
///
/// When the platform is unreliable (no PHP, no overrides), returns an error
/// that points at `--ignore-platform-reqs` or `COMPOSER_PLATFORM_PHP`.
pub fn check_requirements(
    platform: &Platform,
    require: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    check_requirements_filtered(platform, require, &[])
}

/// Like [`check_requirements`], but skips names matching `ignore_patterns`.
pub fn check_requirements_filtered(
    platform: &Platform,
    require: &std::collections::BTreeMap<String, String>,
    ignore_patterns: &[String],
) -> Result<()> {
    let platform_reqs: Vec<_> = require
        .iter()
        .filter(|(name, _)| crate::PackageId::parse(name).is_ok_and(|id| id.is_platform()))
        .filter(|(name, _)| !platform_req_ignored(name, ignore_patterns))
        .collect();

    if platform_reqs.is_empty() {
        return Ok(());
    }

    if !platform.reliable {
        return Err(Error::Resolve(
            "PHP runtime not detected; cannot verify platform requirements \
             (install `php` on PATH, set COMPOSER_PLATFORM_PHP, use config.platform, \
             or pass --ignore-platform-reqs)"
                .into(),
        ));
    }

    for (name, constraint) in platform_reqs {
        if !platform.satisfies(name, constraint) {
            return Err(Error::Resolve(format!(
                "platform requirement {name} ({constraint}) is not satisfied \
                 (PHP {} detected)",
                platform.php.as_str()
            )));
        }
    }
    Ok(())
}

fn detect_php_version() -> Option<ComposerVersion> {
    let output = Command::new("php")
        .args(["-r", "echo PHP_VERSION;"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        return None;
    }
    ComposerVersion::parse(&version).ok()
}

fn detect_php_extensions() -> Option<AHashSet<String>> {
    let output = Command::new("php").args(["-m"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // `php -m` may or may not include section headers depending on SAPI/version.
    let mut exts = AHashSet::new();
    let mut in_modules = false;
    let has_headers = text.lines().any(|l| l.starts_with('['));

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.eq_ignore_ascii_case("[PHP Modules]") {
            in_modules = true;
            continue;
        }
        if line.starts_with('[') {
            in_modules = false;
            continue;
        }
        if !has_headers || in_modules {
            exts.insert(line.to_ascii_lowercase());
        }
    }
    Some(exts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satisfies_php_constraint() {
        let platform = Platform {
            php: ComposerVersion::parse("8.2.0").unwrap(),
            extensions: AHashSet::from_iter(["json".into()]),
            reliable: true,
        };
        assert!(platform.satisfies("php", ">=8.1"));
        assert!(!platform.satisfies("php", ">=8.3"));
        assert!(platform.satisfies("ext-json", "*"));
        assert!(!platform.satisfies("ext-missing", "*"));
    }

    #[test]
    fn unreliable_never_satisfies() {
        let platform = Platform {
            php: ComposerVersion::parse("0.0.0").unwrap(),
            extensions: AHashSet::new(),
            reliable: false,
        };
        assert!(!platform.satisfies("php", ">=7.0"));
        let mut req = std::collections::BTreeMap::new();
        req.insert("php".into(), ">=8.1".into());
        assert!(check_requirements(&platform, &req).is_err());
    }

    #[test]
    fn config_platform_override() {
        let mut platform = Platform {
            php: ComposerVersion::parse("0.0.0").unwrap(),
            extensions: AHashSet::new(),
            reliable: false,
        };
        let config = serde_json::json!({
            "platform": {
                "php": "8.1.0",
                "ext-json": "1"
            }
        });
        platform.apply_config_platform(Some(&config));
        assert!(platform.reliable);
        assert!(platform.satisfies("php", ">=8.1"));
        assert!(platform.satisfies("ext-json", "*"));
    }
}
