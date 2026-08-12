//! Path and VCS repository package discovery.

use composer_core::error::{Error, Result};
use composer_core::{AutoloadConfig, ComposerVersion};
use composer_lock::{DistInfo, LockedPackage, SourceInfo};
use composer_manifest::{
    resolve_path_url, PathPackageManifest, Repository,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Path,
    Vcs,
}

/// A package provided by a local path or VCS repository.
#[derive(Debug, Clone)]
pub struct LocalPathPackage {
    pub name: String,
    pub version: ComposerVersion,
    pub package_type: Option<String>,
    pub require: BTreeMap<String, String>,
    pub autoload: Option<AutoloadConfig>,
    pub description: Option<String>,
    pub bin: Vec<String>,
    pub provide: BTreeMap<String, String>,
    pub replace: BTreeMap<String, String>,
    pub conflict: BTreeMap<String, String>,
    pub source_kind: SourceKind,
    /// Absolute path on disk (path repo) or checkout directory (vcs).
    pub local_path: PathBuf,
    /// Whether path installs should symlink.
    pub symlink: bool,
    /// Original VCS URL if any.
    pub vcs_url: Option<String>,
    pub vcs_reference: Option<String>,
}

impl LocalPathPackage {
    pub fn to_locked(&self) -> LockedPackage {
        let (dist, source) = match self.source_kind {
            SourceKind::Path => (
                Some(DistInfo {
                    dist_type: "path".into(),
                    url: self.local_path.to_string_lossy().into_owned(),
                    reference: None,
                    shasum: None,
                    mirrors: None,
                }),
                Some(SourceInfo {
                    source_type: "path".into(),
                    url: self.local_path.to_string_lossy().into_owned(),
                    reference: None,
                }),
            ),
            SourceKind::Vcs => (
                None,
                Some(SourceInfo {
                    source_type: "git".into(),
                    url: self.vcs_url.clone().unwrap_or_default(),
                    reference: self.vcs_reference.clone(),
                }),
            ),
        };

        LockedPackage {
            name: self.name.clone(),
            version: self.version.raw.clone(),
            source,
            dist,
            require: self.require.clone(),
            require_dev: BTreeMap::new(),
            package_type: self.package_type.clone(),
            extra: Some(serde_json::json!({
                "composer-rs": { "symlink": self.symlink }
            })),
            autoload: self.autoload.clone(),
            autoload_dev: None,
            notification_url: None,
            license: vec![],
            description: self.description.clone(),
            homepage: None,
            keywords: vec![],
            time: None,
            replace: self.replace.clone(),
            provide: self.provide.clone(),
            conflict: self.conflict.clone(),
            suggest: BTreeMap::new(),
            bin: self.bin.clone(),
            abandoned: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct LocalSources {
    pub path_packages: Vec<LocalPathPackage>,
    pub vcs_packages: Vec<LocalPathPackage>,
}

/// Discover packages from path and VCS repositories.
pub fn collect_local_sources(
    project_root: &Path,
    repos: &[Repository],
) -> Result<LocalSources> {
    let mut out = LocalSources::default();
    for repo in repos {
        match repo {
            Repository::Path { url, symlink } => {
                let abs = resolve_path_url(project_root, url);
                if !abs.join("composer.json").is_file() {
                    warn!(path = %abs.display(), "path repository missing composer.json");
                    continue;
                }
                let manifest = PathPackageManifest::load(&abs)?;
                let version_str = manifest
                    .version
                    .clone()
                    .unwrap_or_else(|| "dev-main".into());
                let version = ComposerVersion::parse(&version_str).unwrap_or_else(|_| {
                    ComposerVersion::parse("dev-main").expect("dev-main")
                });
                debug!(name = %manifest.name, path = %abs.display(), "path package");
                out.path_packages.push(LocalPathPackage {
                    name: manifest.name,
                    version,
                    package_type: manifest.package_type,
                    require: manifest.require,
                    autoload: manifest.autoload,
                    description: manifest.description,
                    bin: manifest.bin,
                    provide: manifest.provide,
                    replace: manifest.replace,
                    conflict: manifest.conflict,
                    source_kind: SourceKind::Path,
                    local_path: abs,
                    symlink: *symlink,
                    vcs_url: None,
                    vcs_reference: None,
                });
            }
            Repository::Vcs { url } => {
                match checkout_vcs(url) {
                    Ok(pkg) => out.vcs_packages.push(pkg),
                    Err(e) => {
                        warn!(url = %url, error = %e, "failed to load vcs repository");
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn checkout_vcs(url: &str) -> Result<LocalPathPackage> {
    let cache = composer_cache::cache_root().join("vcs");
    std::fs::create_dir_all(&cache).map_err(|e| Error::io(&cache, e))?;

    let hash = blake3::hash(url.as_bytes());
    let dest = cache.join(hash.to_hex().as_str());

    if dest.join(".git").is_dir() {
        // fetch latest
        let _ = Command::new("git")
            .args(["-C", &dest.to_string_lossy(), "fetch", "--tags", "--force"])
            .status();
    } else {
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(|e| Error::io(&dest, e))?;
        }
        let status = Command::new("git")
            .args(["clone", "--depth", "1", url, &dest.to_string_lossy()])
            .status()
            .map_err(|e| Error::other(format!("git clone failed to start: {e}")))?;
        if !status.success() {
            return Err(Error::other(format!("git clone failed for {url}")));
        }
    }

    let head = Command::new("git")
        .args(["-C", &dest.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        });

    let manifest = PathPackageManifest::load(&dest)?;
    let version_str = manifest
        .version
        .clone()
        .unwrap_or_else(|| "dev-main".into());
    let version =
        ComposerVersion::parse(&version_str).unwrap_or_else(|_| ComposerVersion::parse("dev-main").unwrap());

    Ok(LocalPathPackage {
        name: manifest.name,
        version,
        package_type: manifest.package_type,
        require: manifest.require,
        autoload: manifest.autoload,
        description: manifest.description,
        bin: manifest.bin,
        provide: manifest.provide,
        replace: manifest.replace,
        conflict: manifest.conflict,
        source_kind: SourceKind::Vcs,
        local_path: dest,
        symlink: false,
        vcs_url: Some(url.to_string()),
        vcs_reference: head,
    })
}
