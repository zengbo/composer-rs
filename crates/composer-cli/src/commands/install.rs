//! `composer-rs install`

use super::{format_duration, header, info, project_paths, success, vendor_dir, warning};
use anyhow::{bail, Context, Result};
use clap::Args;
use composer_autoload::{generate, AutoloadOptions};
use composer_download::{default_concurrency, PackageInstaller};
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;
use composer_repo::RepositoryClient;
use composer_resolver::{locked_list, resolve, ResolveOptions};
use std::time::Instant;

#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    /// Skip require-dev packages
    #[arg(long)]
    pub no_dev: bool,

    /// Do not install; only show what would happen
    #[arg(long)]
    pub dry_run: bool,

    /// Prefer dist archives (default)
    #[arg(long, default_value_t = true)]
    pub prefer_dist: bool,

    /// Optimize PSR autoload into classmap
    #[arg(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Authoritative classmap
    #[arg(short = 'a', long)]
    pub classmap_authoritative: bool,

    /// Max concurrent downloads (default: adaptive)
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Verify dist shasum when present
    #[arg(long)]
    pub verify_checksums: bool,

    /// Ignore platform requirements (always ignored for install path today)
    #[arg(long)]
    pub ignore_platform_reqs: bool,
}

pub async fn run(args: InstallArgs) -> Result<()> {
    let start = Instant::now();
    header("Installing dependencies from lock file");

    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found in {}", cwd.display());
    }

    let manifest = ComposerJson::load(&json_path).context("parse composer.json")?;
    let vendor = vendor_dir(&manifest, &cwd);
    let with_dev = !args.no_dev;
    let concurrency = args.concurrency.unwrap_or_else(default_concurrency);

    let lock = if lock_path.exists() {
        info(&format!("Lock file: {}", lock_path.display()));
        ComposerLock::load(&lock_path).context("parse composer.lock")?
    } else {
        warning("No composer.lock found — resolving from composer.json");
        let client = RepositoryClient::new()?;
        let options = ResolveOptions {
            with_dev,
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: manifest.minimum_stability().to_string(),
            concurrency,
        };
        let resolution = resolve(&client, &manifest, &options, &cwd)
            .await
            .context("dependency resolution (PubGrub)")?;
        let lock = resolution.to_lock(&manifest);
        if !args.dry_run {
            lock.save(&lock_path)?;
            success(&format!("Wrote {}", lock_path.display()));
        }
        lock
    };

    let packages = locked_list(&lock, with_dev);
    let installer_paths = manifest.installer_paths();
    if !installer_paths.is_empty() {
        info("Using custom installer-paths from composer.json extra");
    }
    info(&format!(
        "{} package(s) to install  ·  concurrency={concurrency}",
        packages.len()
    ));

    if args.dry_run {
        for p in &packages {
            let dest = installer_paths
                .resolve_relative(&p.name, p.package_type.as_deref())
                .unwrap_or_else(|| format!("vendor/{}", p.name));
            println!("  - {} ({}) → {dest}", p.name, p.version);
        }
        success("Dry run complete (no changes)");
        return Ok(());
    }

    std::fs::create_dir_all(&vendor)?;

    let installer = PackageInstaller::new(concurrency, args.verify_checksums)?
        .with_project_root(&cwd)
        .with_installer_paths(installer_paths);
    let refs: Vec<&composer_lock::LockedPackage> = packages.iter().collect();
    installer
        .install_all(&refs, &vendor)
        .await
        .context("install packages")?;

    let stats = installer.stats().snapshot();
    info(&format!(
        "cache hits: {}  downloads: {}  hardlinks: {}  copies: {}  bytes: {}",
        stats.cache_hits,
        stats.downloaded,
        stats.hardlinks,
        stats.copies,
        composer_cache::format_bytes(stats.bytes),
    ));

    // Autoloader
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
    success("Autoloader generated");

    // Marker for tooling
    let marker = vendor.join(".composer-rs-installed");
    std::fs::write(
        &marker,
        format!(
            "{{\n  \"packages\": {},\n  \"cache_hits\": {},\n  \"downloaded\": {}\n}}\n",
            stats.total, stats.cache_hits, stats.downloaded
        ),
    )?;

    success(&format!(
        "Installed {} packages in {}",
        packages.len(),
        format_duration(start.elapsed())
    ));
    Ok(())
}
