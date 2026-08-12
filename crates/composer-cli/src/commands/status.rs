//! `composer-rs status` — verify installed packages against the lockfile.

use super::{info, project_paths, success, vendor_dir, warning};
use anyhow::{Result, bail};
use clap::Args;
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;
use serde_json::Value;
use std::fs;
use std::path::Path;

#[derive(Args, Debug, Clone)]
pub struct StatusArgs {
    #[arg(long)]
    pub no_dev: bool,

    /// Exit 1 if any issue is found
    #[arg(long)]
    pub strict: bool,
}

#[derive(Debug, Default)]
struct Issues {
    missing: Vec<String>,
    modified: Vec<String>,
    version_mismatch: Vec<String>,
    extra: Vec<String>,
}

impl Issues {
    fn total(&self) -> usize {
        self.missing.len() + self.modified.len() + self.version_mismatch.len() + self.extra.len()
    }
}

pub fn run(args: StatusArgs) -> Result<()> {
    let (cwd, json_path, lock_path) = project_paths()?;
    if !lock_path.exists() {
        bail!("composer.lock not found");
    }
    let manifest = ComposerJson::load(&json_path)?;
    let lock = ComposerLock::load(&lock_path)?;
    let vendor = vendor_dir(&manifest, &cwd);
    let paths = manifest.installer_paths();
    let with_dev = !args.no_dev;

    let mut issues = Issues::default();
    let mut expected_dirs: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();

    for pkg in lock.packages_to_install(with_dev) {
        if pkg.is_metapackage() {
            continue;
        }
        let dest = paths
            .resolve(&cwd, &pkg.name, pkg.package_type.as_deref())
            .unwrap_or_else(|| vendor.join(&pkg.name));
        expected_dirs.insert(dest.clone());

        // Path packages are editable by design
        if pkg.dist.as_ref().is_some_and(|d| d.dist_type == "path")
            || pkg.source.as_ref().is_some_and(|s| s.source_type == "path")
        {
            if !dest.exists() {
                warning(&format!(
                    "{}: path package missing ({})",
                    pkg.name,
                    dest.display()
                ));
                issues.missing.push(pkg.name.clone());
            }
            continue;
        }

        if !dest.exists() {
            warning(&format!("{}: missing ({})", pkg.name, dest.display()));
            issues.missing.push(pkg.name.clone());
            continue;
        }

        // Marker written by installer
        let marker = dest.join(".composer-rs-installed");
        if marker.is_file() {
            match fs::read_to_string(&marker) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(doc) => {
                        let ver = doc.get("version").and_then(|v| v.as_str());
                        let key = doc.get("cache_key").and_then(|v| v.as_str());
                        if ver.is_some_and(|v| v != pkg.version) {
                            warning(&format!(
                                "{}: installed version {} != lock {}",
                                pkg.name,
                                ver.unwrap_or("?"),
                                pkg.version
                            ));
                            issues.version_mismatch.push(pkg.name.clone());
                        }
                        let expected_key = pkg.cache_key();
                        if key.is_some_and(|k| k != expected_key) {
                            warning(&format!(
                                "{}: install marker cache_key mismatch (package may have been replaced)",
                                pkg.name
                            ));
                            issues.modified.push(pkg.name.clone());
                        }
                    }
                    Err(_) => {
                        warning(&format!("{}: corrupt install marker", pkg.name));
                        issues.modified.push(pkg.name.clone());
                    }
                },
                Err(_) => {
                    warning(&format!("{}: unreadable install marker", pkg.name));
                    issues.modified.push(pkg.name.clone());
                }
            }
        } else {
            // Fallback precision without marker: composer.json name/version
            let cj = dest.join("composer.json");
            if cj.is_file() {
                if let Ok(text) = fs::read_to_string(&cj) {
                    if let Ok(doc) = serde_json::from_str::<Value>(&text) {
                        if let Some(n) = doc.get("name").and_then(|v| v.as_str()) {
                            if n != pkg.name {
                                warning(&format!(
                                    "{}: composer.json name is `{n}` (expected {})",
                                    pkg.name, pkg.name
                                ));
                                issues.modified.push(pkg.name.clone());
                            }
                        }
                        if let Some(v) = doc.get("version").and_then(|v| v.as_str()) {
                            let lock_v = pkg.version.trim_start_matches('v');
                            let file_v = v.trim_start_matches('v');
                            if file_v != lock_v && !pkg.version.starts_with("dev-") {
                                warning(&format!(
                                    "{}: composer.json version `{v}` != lock `{}`",
                                    pkg.name, pkg.version
                                ));
                                issues.version_mismatch.push(pkg.name.clone());
                            }
                        }
                    }
                }
            } else if !dest.is_symlink() {
                warning(&format!(
                    "{}: no composer.json and no install marker",
                    pkg.name
                ));
                issues.modified.push(pkg.name.clone());
            }
        }

        // Unexpected local git checkout for dist installs
        if dest.join(".git").exists()
            && !pkg
                .source
                .as_ref()
                .is_some_and(|s| s.source_type == "git" || s.source_type == "vcs")
        {
            warning(&format!("{}: unexpected .git directory", pkg.name));
            issues.modified.push(pkg.name.clone());
        }
    }

    // Extra vendor packages not in lock (best-effort under vendor/*/*)
    if vendor.is_dir() {
        for vendor_entry in fs::read_dir(&vendor).into_iter().flatten().flatten() {
            let vname = vendor_entry.file_name();
            let vname = vname.to_string_lossy();
            if vname == "bin" || vname == "composer" || vname.starts_with('.') {
                continue;
            }
            let vpath = vendor_entry.path();
            if !vpath.is_dir() {
                continue;
            }
            for pkg_entry in fs::read_dir(&vpath).into_iter().flatten().flatten() {
                let ppath = pkg_entry.path();
                if !ppath.is_dir() && !ppath.is_symlink() {
                    continue;
                }
                if !expected_dirs.contains(&ppath) {
                    let name = format!("{}/{}", vname, pkg_entry.file_name().to_string_lossy());
                    // only report if it looks like a package
                    if ppath.join("composer.json").exists()
                        || ppath.join(".composer-rs-installed").exists()
                    {
                        warning(&format!("{name}: present in vendor but not in lock"));
                        issues.extra.push(name);
                    }
                }
            }
        }
    }

    let total = issues.total();
    if total == 0 {
        success("Vendor matches lockfile (markers + package metadata OK)");
    } else {
        info(&format!(
            "{total} issue(s): missing={} modified={} version={} extra={}",
            issues.missing.len(),
            issues.modified.len(),
            issues.version_mismatch.len(),
            issues.extra.len()
        ));
        if args.strict {
            bail!("status found {total} issue(s) (--strict)");
        }
    }
    let _ = Path::new(".");
    Ok(())
}
