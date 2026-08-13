//! `composer-rs reinstall`

use super::{format_duration, header, info, project_paths, success, vendor_dir};
use crate::hooks::{link_bins, run_lifecycle};
use anyhow::{Context, Result, bail};
use clap::Args;
use composer_auth::AuthStore;
use composer_autoload::{AutoloadOptions, generate};
use composer_download::{PackageInstaller, default_concurrency};
// link_bins via hooks
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;
use composer_scripts::ScriptEvent;
use std::time::Instant;

#[derive(Args, Debug, Clone)]
pub struct ReinstallArgs {
    /// Packages to reinstall (default: all locked packages)
    pub packages: Vec<String>,

    #[arg(long)]
    pub no_dev: bool,

    #[arg(long)]
    pub concurrency: Option<usize>,

    #[arg(long)]
    pub prefer_source: bool,

    #[arg(long)]
    pub verify_checksums: bool,

    #[arg(long)]
    pub no_autoloader: bool,

    #[arg(long)]
    pub no_scripts: bool,

    #[arg(short = 'o', long)]
    pub optimize_autoloader: bool,

    #[arg(short = 'a', long)]
    pub classmap_authoritative: bool,
}

pub async fn run(args: ReinstallArgs) -> Result<()> {
    let start = Instant::now();
    header("Reinstalling packages");

    let (cwd, json_path, lock_path) = project_paths()?;
    if !lock_path.exists() {
        bail!("composer.lock not found");
    }
    let manifest = ComposerJson::load(&json_path)?;
    let lock = ComposerLock::load(&lock_path)?;
    let vendor = vendor_dir(&manifest, &cwd);
    let with_dev = !args.no_dev;
    let installer_paths = manifest.installer_paths();

    let mut targets: Vec<_> = lock.packages_to_install(with_dev);
    if !args.packages.is_empty() {
        targets.retain(|p| args.packages.iter().any(|n| n == &p.name));
        if targets.is_empty() {
            bail!("no matching packages in lock");
        }
    }

    for pkg in &targets {
        let dest = installer_paths
            .resolve(&cwd, &pkg.name, pkg.package_type.as_deref())
            .unwrap_or_else(|| vendor.join(pkg.name.as_str()));
        if dest.exists() {
            info(&format!("Removing {}", dest.display()));
            if dest.is_dir() {
                std::fs::remove_dir_all(&dest)
                    .with_context(|| format!("remove {}", dest.display()))?;
            } else {
                let _ = std::fs::remove_file(&dest);
            }
        }
    }

    let concurrency = args.concurrency.unwrap_or_else(default_concurrency);
    let auth = AuthStore::load(Some(&cwd)).unwrap_or_default();
    let prefer_dist = manifest.resolve_prefer_dist(true, args.prefer_source);
    let installer = PackageInstaller::new(concurrency, args.verify_checksums)?
        .with_project_root(&cwd)
        .with_installer_paths(installer_paths.clone())
        .with_prefer_dist(prefer_dist)
        .with_auth(auth);
    let refs: Vec<_> = targets.iter().copied().collect();
    installer.install_all(&refs, &vendor).await?;
    super::warn_copy_install(installer.stats().snapshot().copies);

    link_bins(&refs, &vendor, &cwd, &manifest, &installer_paths)?;

    if !args.no_autoloader {
        run_lifecycle(
            &manifest,
            ScriptEvent::PreAutoloadDump,
            &cwd,
            args.no_scripts,
            with_dev,
        )?;
        // Regenerate for full lock set so autoload stays consistent.
        let all = lock.packages_to_install(with_dev);
        let _ = all;
        generate(
            &cwd,
            &vendor,
            &manifest,
            Some(&lock),
            &AutoloadOptions {
                optimize: args.optimize_autoloader,
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
        success("Autoloader regenerated");
    }

    success(&format!(
        "Reinstalled {} package(s) in {}",
        targets.len(),
        format_duration(start.elapsed())
    ));
    Ok(())
}
