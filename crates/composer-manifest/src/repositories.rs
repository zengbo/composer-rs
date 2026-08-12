//! Composer `repositories` configuration (path, vcs, composer/packagist).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// A configured package repository.
#[derive(Debug, Clone)]
pub enum Repository {
    /// Packagist-compatible Composer repository.
    Composer { url: String },
    /// Local path package.
    Path {
        url: PathBuf,
        /// If true (default), symlink into vendor instead of copy.
        symlink: bool,
    },
    /// Git (or other VCS) package source.
    Vcs { url: String },
    /// Explicit package definition (inline).
    Package { packages: Vec<Value> },
}

/// Parse `repositories` from composer.json (array or object map).
pub fn parse_repositories(value: Option<&Value>) -> Vec<Repository> {
    let Some(value) = value else {
        return Vec::new();
    };

    match value {
        Value::Array(arr) => arr.iter().filter_map(parse_one).collect(),
        Value::Object(map) => {
            // Composer also allows `{ "packagist.org": false, "my-repo": { ... } }`
            let mut out = Vec::new();
            for (key, val) in map {
                if val.as_bool() == Some(false) {
                    continue;
                }
                if let Some(repo) = parse_one(val) {
                    // named packagist disable is handled above
                    if key == "packagist.org" || key == "packagist" {
                        continue;
                    }
                    out.push(repo);
                } else if let Some(url) = val.as_str() {
                    out.push(Repository::Composer {
                        url: url.to_string(),
                    });
                }
                let _ = key;
            }
            out
        }
        _ => Vec::new(),
    }
}

fn parse_one(value: &Value) -> Option<Repository> {
    let obj = value.as_object()?;
    let ty = obj.get("type")?.as_str()?;

    match ty {
        "composer" => {
            let url = obj.get("url")?.as_str()?.to_string();
            Some(Repository::Composer { url })
        }
        "path" => {
            let url = obj.get("url")?.as_str()?;
            let symlink = obj
                .get("options")
                .and_then(|o| o.get("symlink"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            Some(Repository::Path {
                url: PathBuf::from(url),
                symlink,
            })
        }
        "vcs" | "git" | "github" | "gitlab" | "bitbucket" => {
            let url = obj.get("url")?.as_str()?.to_string();
            Some(Repository::Vcs { url })
        }
        "package" => {
            let packages = match obj.get("package") {
                Some(Value::Array(a)) => a.clone(),
                Some(p) => vec![p.clone()],
                None => return None,
            };
            Some(Repository::Package { packages })
        }
        _ => None,
    }
}

/// Resolve a path repository URL relative to the project root.
pub fn resolve_path_url(project_root: &Path, url: &Path) -> PathBuf {
    if url.is_absolute() {
        url.to_path_buf()
    } else {
        project_root.join(url)
    }
}

/// Lightweight view of a path package's composer.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathPackageManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(rename = "type", default)]
    pub package_type: Option<String>,
    #[serde(default)]
    pub require: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub autoload: Option<composer_core::AutoloadConfig>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub bin: Vec<String>,
    #[serde(default)]
    pub provide: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub replace: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub conflict: std::collections::BTreeMap<String, String>,
}

impl PathPackageManifest {
    pub fn load(dir: &Path) -> composer_core::Result<Self> {
        let path = dir.join("composer.json");
        let text =
            std::fs::read_to_string(&path).map_err(|e| composer_core::Error::io(&path, e))?;
        serde_json::from_str(&text)
            .map_err(|e| composer_core::Error::Manifest(format!("{}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_path_and_vcs() {
        let repos = parse_repositories(Some(&json!([
            { "type": "path", "url": "../libs/foo" },
            { "type": "vcs", "url": "https://github.com/acme/bar.git" },
            { "type": "composer", "url": "https://repo.packagist.org" }
        ])));
        assert_eq!(repos.len(), 3);
        assert!(matches!(&repos[0], Repository::Path { .. }));
        assert!(matches!(&repos[1], Repository::Vcs { url } if url.contains("github")));
    }
}
