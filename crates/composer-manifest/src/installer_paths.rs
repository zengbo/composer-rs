//! `extra.installer-paths` — install packages outside the default vendor tree.
//!
//! ```json
//! {
//!   "extra": {
//!     "installer-paths": {
//!       "wp-content/plugins/{$name}/": ["type:wordpress-plugin"],
//!       "modules/{$name}/": ["type:drupal-module"],
//!       "custom/": ["acme/special"]
//!     }
//!   }
//! }
//! ```

use serde_json::Value;
use std::path::{Path, PathBuf};

/// Parsed installer-paths rules from `composer.json`.
#[derive(Debug, Clone, Default)]
pub struct InstallerPaths {
    rules: Vec<(String, Vec<PathMatcher>)>,
}

#[derive(Debug, Clone)]
enum PathMatcher {
    Type(String),
    Package(String),
    Vendor(String),
}

impl InstallerPaths {
    pub fn from_extra(extra: Option<&Value>) -> Self {
        let mut rules = Vec::new();
        let Some(extra) = extra.and_then(|e| e.as_object()) else {
            return Self { rules };
        };
        let Some(paths) = extra.get("installer-paths").and_then(|p| p.as_object()) else {
            return Self { rules };
        };

        for (template, matchers) in paths {
            let Some(arr) = matchers.as_array() else {
                continue;
            };
            let parsed: Vec<PathMatcher> = arr
                .iter()
                .filter_map(|m| m.as_str().and_then(parse_matcher))
                .collect();
            if !parsed.is_empty() {
                rules.push((template.clone(), parsed));
            }
        }
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Resolve install path relative to project root, or `None` for default `vendor/name`.
    pub fn resolve(
        &self,
        project_root: &Path,
        package_name: &str,
        package_type: Option<&str>,
    ) -> Option<PathBuf> {
        for (template, matchers) in &self.rules {
            if matchers
                .iter()
                .any(|m| m.matches(package_name, package_type))
            {
                let rel = expand_template(template, package_name);
                return Some(project_root.join(rel));
            }
        }
        None
    }

    /// Path relative to project root (for lock metadata / display).
    pub fn resolve_relative(
        &self,
        package_name: &str,
        package_type: Option<&str>,
    ) -> Option<String> {
        for (template, matchers) in &self.rules {
            if matchers
                .iter()
                .any(|m| m.matches(package_name, package_type))
            {
                return Some(expand_template(template, package_name));
            }
        }
        None
    }
}

impl PathMatcher {
    fn matches(&self, package_name: &str, package_type: Option<&str>) -> bool {
        match self {
            Self::Type(t) => package_type.is_some_and(|pt| pt == t),
            Self::Package(p) => package_name == p,
            Self::Vendor(v) => package_name
                .strip_prefix(v)
                .is_some_and(|rest| rest.starts_with('/')),
        }
    }
}

fn parse_matcher(s: &str) -> Option<PathMatcher> {
    if let Some(t) = s.strip_prefix("type:") {
        return Some(PathMatcher::Type(t.to_string()));
    }
    if let Some(v) = s.strip_suffix("/*") {
        return Some(PathMatcher::Vendor(v.to_string()));
    }
    if s.contains('/') {
        return Some(PathMatcher::Package(s.to_string()));
    }
    None
}

fn expand_template(template: &str, package_name: &str) -> String {
    let (vendor, name) = package_name
        .split_once('/')
        .unwrap_or(("", package_name));
    template
        .replace("{$name}", name)
        .replace("{$vendor}", vendor)
        .replace("{$package}", package_name)
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn type_and_package_rules() {
        let extra = json!({
            "installer-paths": {
                "wp-content/plugins/{$name}": ["type:wordpress-plugin"],
                "custom/loc": ["acme/special"],
                "vendor-plugins/{$name}": ["wpackagist-plugin/*"]
            }
        });
        let paths = InstallerPaths::from_extra(Some(&extra));
        let root = Path::new("/proj");

        // type: match (unique type, no overlapping vendor rule needed)
        assert_eq!(
            paths.resolve(root, "vendor/akismet", Some("wordpress-plugin")),
            Some(PathBuf::from("/proj/wp-content/plugins/akismet"))
        );
        // exact package name
        assert_eq!(
            paths.resolve(root, "acme/special", Some("library")),
            Some(PathBuf::from("/proj/custom/loc"))
        );
        // vendor/* prefix
        assert_eq!(
            paths.resolve(root, "wpackagist-plugin/foo", Some("library")),
            Some(PathBuf::from("/proj/vendor-plugins/foo"))
        );
        assert!(paths
            .resolve(root, "symfony/console", Some("library"))
            .is_none());
    }
}
