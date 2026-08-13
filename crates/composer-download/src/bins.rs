//! Install package binaries into `config.bin-dir` (default `vendor/bin`).

use composer_core::error::{Error, Result};
use composer_lock::LockedPackage;
use composer_manifest::InstallerPaths;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Result of linking package binaries.
#[derive(Debug, Default)]
pub struct BinInstallResult {
    pub linked: usize,
    /// Human-readable conflict warnings (same basename from multiple packages).
    pub conflicts: Vec<String>,
}

/// Link package `bin` entries into `bin_dir` (relative paths resolved from package install root).
///
/// When two packages declare the same binary basename, the later package wins and a warning
/// is recorded (Composer-style last-write-wins with visibility).
pub fn install_bins(
    packages: &[&LockedPackage],
    vendor_dir: &Path,
    project_root: &Path,
    bin_dir: &Path,
    installer_paths: &InstallerPaths,
) -> Result<BinInstallResult> {
    fs::create_dir_all(bin_dir).map_err(|e| Error::io(bin_dir, e))?;
    let mut result = BinInstallResult::default();
    // basename → package that currently owns the link
    let mut owners: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for pkg in packages {
        if pkg.bin.is_empty() || pkg.is_metapackage() {
            continue;
        }
        let pkg_root = package_install_path(pkg, vendor_dir, project_root, installer_paths)?;
        for bin_rel in &pkg.bin {
            let src = pkg_root.join(bin_rel);
            if !src.exists() {
                warn!(
                    package = %pkg.name,
                    bin = %bin_rel,
                    path = %src.display(),
                    "bin target missing; skip"
                );
                continue;
            }
            let name = Path::new(bin_rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(bin_rel.as_str())
                .to_string();
            let dest = bin_dir.join(&name);
            if let Some(prev) = owners.get(&name) {
                if prev != &pkg.name {
                    let msg = format!(
                        "binary `{name}` conflict: was provided by {prev}, overwritten by {}",
                        pkg.name
                    );
                    warn!("{}", msg);
                    result.conflicts.push(msg);
                }
            }
            if dest.exists() || dest.is_symlink() {
                let _ = fs::remove_file(&dest);
            }
            link_or_copy(&src, &dest)?;
            // Best-effort executable bit on Unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&dest) {
                    let mut perms = meta.permissions();
                    perms.set_mode(perms.mode() | 0o755);
                    let _ = fs::set_permissions(&dest, perms);
                }
            }
            owners.insert(name.clone(), pkg.name.clone());
            debug!(package = %pkg.name, bin = %name, "linked binary");
            result.linked += 1;
        }
    }
    Ok(result)
}

fn package_install_path(
    pkg: &LockedPackage,
    vendor_dir: &Path,
    project_root: &Path,
    installer_paths: &InstallerPaths,
) -> Result<PathBuf> {
    if let Some(custom) =
        installer_paths.resolve(project_root, &pkg.name, pkg.package_type.as_deref())
    {
        return Ok(custom);
    }
    Ok(vendor_dir.join(pkg.package_id()?.install_path()))
}

fn link_or_copy(src: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // Prefer relative symlink from bin_dir to package file.
        use std::os::unix::fs::symlink;
        if let Some(parent) = dest.parent() {
            if let Ok(rel) = pathdiff_relative(parent, src) {
                if symlink(&rel, dest).is_ok() {
                    return Ok(());
                }
            }
        }
        if symlink(src, dest).is_ok() {
            return Ok(());
        }
    }
    fs::copy(src, dest).map_err(|e| Error::io(dest, e))?;
    Ok(())
}

/// Minimal relative path helper (no extra dep).
fn pathdiff_relative(from_dir: &Path, to: &Path) -> Result<PathBuf> {
    let from = fs::canonicalize(from_dir).unwrap_or_else(|_| from_dir.to_path_buf());
    let to = fs::canonicalize(to).unwrap_or_else(|_| to.to_path_buf());
    let from_comps: Vec<_> = from.components().collect();
    let to_comps: Vec<_> = to.components().collect();
    let mut i = 0;
    while i < from_comps.len() && i < to_comps.len() && from_comps[i] == to_comps[i] {
        i += 1;
    }
    let mut rel = PathBuf::new();
    for _ in i..from_comps.len() {
        rel.push("..");
    }
    for c in &to_comps[i..] {
        rel.push(c.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    Ok(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use composer_lock::{DistInfo, LockedPackage};
    use std::collections::BTreeMap;

    fn pkg(name: &str, bins: &[&str]) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: "1.0.0".into(),
            source: None,
            dist: Some(DistInfo {
                dist_type: "path".into(),
                url: ".".into(),
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
            bin: bins.iter().map(|s| (*s).to_string()).collect(),
            abandoned: None,
        }
    }

    #[test]
    fn links_bin_into_vendor_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path().join("vendor");
        let pkg_dir = vendor.join("acme/tool");
        fs::create_dir_all(pkg_dir.join("bin")).unwrap();
        fs::write(pkg_dir.join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();

        let p = pkg("acme/tool", &["bin/hello"]);
        let bin_dir = vendor.join("bin");
        let n = install_bins(
            &[&p],
            &vendor,
            tmp.path(),
            &bin_dir,
            &InstallerPaths::default(),
        )
        .unwrap();
        assert_eq!(n.linked, 1);
        assert!(bin_dir.join("hello").exists() || bin_dir.join("hello").is_symlink());
    }

    #[test]
    fn warns_on_bin_name_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path().join("vendor");
        for (name, body) in [("acme/a", "a"), ("acme/b", "b")] {
            let d = vendor.join(name).join("bin");
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("tool"), body).unwrap();
        }
        let a = pkg("acme/a", &["bin/tool"]);
        let b = pkg("acme/b", &["bin/tool"]);
        let res = install_bins(
            &[&a, &b],
            &vendor,
            tmp.path(),
            &vendor.join("bin"),
            &InstallerPaths::default(),
        )
        .unwrap();
        assert_eq!(res.linked, 2);
        assert!(!res.conflicts.is_empty());
        let body = fs::read_to_string(vendor.join("bin/tool")).unwrap();
        assert_eq!(body, "b");
    }

    #[test]
    fn reinstall_does_not_warn_when_bin_already_linked() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path().join("vendor");
        let pkg_dir = vendor.join("acme/tool");
        fs::create_dir_all(pkg_dir.join("bin")).unwrap();
        fs::write(pkg_dir.join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();
        let p = pkg("acme/tool", &["bin/hello"]);
        let bin_dir = vendor.join("bin");
        let paths = InstallerPaths::default();
        install_bins(&[&p], &vendor, tmp.path(), &bin_dir, &paths).unwrap();
        let again = install_bins(&[&p], &vendor, tmp.path(), &bin_dir, &paths).unwrap();
        assert_eq!(again.linked, 1);
        assert!(again.conflicts.is_empty());
    }
}
