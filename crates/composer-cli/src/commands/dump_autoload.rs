//! `composer-rs dump-autoload`

use super::{header, project_paths, success, vendor_dir};
use crate::hooks::run_lifecycle;
use anyhow::{Context, Result, bail};
use clap::Args;
use composer_autoload::{AutoloadOptions, generate};
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;
use composer_scripts::ScriptEvent;
use std::path::Path;

#[derive(Args, Debug, Clone)]
pub struct DumpAutoloadArgs {
    #[arg(short = 'o', long)]
    pub optimize: bool,

    #[arg(short = 'a', long)]
    pub classmap_authoritative: bool,

    #[arg(long)]
    pub no_dev: bool,

    #[arg(long)]
    pub no_scripts: bool,
}

pub fn run(args: DumpAutoloadArgs) -> Result<()> {
    header("Generating autoload files");
    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    let manifest = ComposerJson::load(&json_path)?;
    let vendor = vendor_dir(&manifest, &cwd);
    let with_dev = !args.no_dev;
    let lock = lock_for_autoload(&lock_path, &vendor)?;

    run_lifecycle(
        &manifest,
        ScriptEvent::PreAutoloadDump,
        &cwd,
        args.no_scripts,
        with_dev,
    )?;
    generate(
        &cwd,
        &vendor,
        &manifest,
        lock.as_ref(),
        &AutoloadOptions {
            optimize: args.optimize,
            classmap_authoritative: args.classmap_authoritative,
            with_dev,
        },
    )?;
    run_lifecycle(
        &manifest,
        ScriptEvent::PostAutoloadDump,
        &cwd,
        args.no_scripts,
        with_dev,
    )?;
    success("Autoloader dumped");
    Ok(())
}

/// Composer reads `vendor/composer/installed.json` when `composer.lock` is gone
/// (some monorepos delete the lock after install). Do not silently dump a
/// root-only autoloader over an existing vendor tree.
pub(crate) fn lock_for_autoload(lock_path: &Path, vendor: &Path) -> Result<Option<ComposerLock>> {
    if lock_path.exists() {
        return ComposerLock::load(lock_path)
            .map(Some)
            .context("parse composer.lock");
    }
    let installed = vendor.join("composer/installed.json");
    if installed.exists() {
        return ComposerLock::load_installed_json(&installed)
            .map(Some)
            .with_context(|| format!("parse {}", installed.display()));
    }
    if vendor_has_packages(vendor) {
        bail!(
            "composer.lock not found and {} is missing; refusing to rewrite autoload over an existing vendor (that would drop installed packages)",
            installed.display()
        );
    }
    Ok(None)
}

fn vendor_has_packages(vendor: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(vendor) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        e.path().is_dir() && name != "composer" && name != "bin"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_lock_reads_installed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path().join("vendor/composer");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(
            vendor.join("installed.json"),
            r#"{
                "packages": [
                    {"name":"thecodingmachine/safe","version":"2.5.0"}
                ],
                "dev": false,
                "dev-package-names": []
            }"#,
        )
        .unwrap();
        let lock = lock_for_autoload(
            &tmp.path().join("composer.lock"),
            &tmp.path().join("vendor"),
        )
        .unwrap()
        .expect("installed.json");
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "thecodingmachine/safe");
    }

    #[test]
    fn missing_lock_and_installed_errors_if_vendor_populated() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path().join("vendor");
        std::fs::create_dir_all(vendor.join("thecodingmachine/safe")).unwrap();
        let err = lock_for_autoload(&tmp.path().join("composer.lock"), &vendor).unwrap_err();
        assert!(
            err.to_string().contains("refusing to rewrite autoload"),
            "{err}"
        );
    }

    #[test]
    fn missing_lock_empty_vendor_is_root_only() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path().join("vendor");
        std::fs::create_dir_all(vendor.join("composer")).unwrap();
        assert!(
            lock_for_autoload(&tmp.path().join("composer.lock"), &vendor)
                .unwrap()
                .is_none()
        );
    }
}
