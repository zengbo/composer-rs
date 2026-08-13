//! Shared post-install hooks: bins, scripts, plugin-skip warnings.

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
    let _ = manifest;
    let plugins: Vec<&str> = lock
        .packages_to_install(with_dev)
        .into_iter()
        .filter(|pkg| pkg.package_type.as_deref() == Some("composer-plugin"))
        .map(|pkg| pkg.name.as_str())
        .collect();
    if plugins.is_empty() {
        return;
    }
    warning(&format!(
        "composer-plugin package(s) not executed: {}",
        plugins.join(", ")
    ));
    warning("composer-rs does not run Composer plugins; config.allow-plugins has no effect.");
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
