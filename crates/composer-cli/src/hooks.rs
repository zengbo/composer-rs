//! Shared post-install hooks: bins, scripts, allow-plugins warnings.

use anyhow::Result;
use composer_download::install_bins;
use composer_lock::{ComposerLock, LockedPackage};
use composer_manifest::{ComposerJson, InstallerPaths};
use composer_scripts::{ScriptEvent, run_event};
use std::path::Path;

use crate::commands::{info, warning};

pub fn link_bins(
    packages: &[&LockedPackage],
    vendor: &Path,
    project_root: &Path,
    manifest: &ComposerJson,
    installer_paths: &InstallerPaths,
) -> Result<()> {
    let bin_dir = project_root.join(manifest.bin_dir());
    let result = install_bins(packages, vendor, project_root, &bin_dir, installer_paths)?;
    for c in &result.conflicts {
        warning(c);
    }
    if result.linked > 0 {
        info(&format!(
            "Linked {} binary(ies) → {}",
            result.linked,
            bin_dir.display()
        ));
    }
    Ok(())
}

pub fn warn_unapproved_plugins(lock: &ComposerLock, manifest: &ComposerJson, with_dev: bool) {
    let allow = manifest.allow_plugins();
    let allow_all = allow.get("*").copied().unwrap_or(false);
    let mut unapproved = Vec::new();
    for pkg in lock.packages_to_install(with_dev) {
        if pkg.package_type.as_deref() != Some("composer-plugin") {
            continue;
        }
        let ok = allow_all || allow.get(&pkg.name).copied().unwrap_or(false);
        if !ok {
            unapproved.push(pkg.name.clone());
        }
    }
    if !unapproved.is_empty() {
        warning(&format!(
            "composer-plugin package(s) not allowed by config.allow-plugins: {}",
            unapproved.join(", ")
        ));
        warning("Plugins are not executed by composer-rs; grant allow-plugins to silence.");
    }
}

pub fn run_lifecycle(
    manifest: &ComposerJson,
    event: ScriptEvent,
    project_root: &Path,
    no_scripts: bool,
    with_dev: bool,
) -> Result<()> {
    if no_scripts {
        return Ok(());
    }
    run_event(manifest, event, project_root, with_dev)?;
    Ok(())
}
