//! `composer-rs update`

use super::{format_duration, header, info, project_paths, success, vendor_dir};
use anyhow::{bail, Context, Result};
use clap::Args;
use composer_autoload::{generate, AutoloadOptions};
use composer_download::{default_concurrency, PackageInstaller};
use composer_manifest::ComposerJson;
use composer_repo::RepositoryClient;
use composer_resolver::{resolve, ResolveOptions};
use std::time::Instant;

#[derive(Args, Debug, Clone)]
pub struct UpdateArgs {
    /// Packages to update (default: all)
    pub packages: Vec<String>,

    #[arg(long)]
    pub no_dev: bool,

    #[arg(long)]
    pub prefer_lowest: bool,

    #[arg(long)]
    pub prefer_stable: bool,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(short = 'o', long)]
    pub optimize_autoloader: bool,

    #[arg(long)]
    pub concurrency: Option<usize>,

    #[arg(long)]
    pub verify_checksums: bool,
}

pub async fn run(args: UpdateArgs) -> Result<()> {
    let start = Instant::now();
    header("Updating dependencies");

    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    if !args.packages.is_empty() {
        info("Partial updates are not fully supported yet — updating entire tree");
    }

    let manifest = ComposerJson::load(&json_path)?;
    let vendor = vendor_dir(&manifest, &cwd);
    let concurrency = args.concurrency.unwrap_or_else(default_concurrency);
    let with_dev = !args.no_dev;

    let client = RepositoryClient::new()?;
    let options = ResolveOptions {
        with_dev,
        prefer_stable: args.prefer_stable || manifest.prefer_stable(),
        prefer_lowest: args.prefer_lowest,
        minimum_stability: manifest.minimum_stability().to_string(),
        concurrency,
    };

    let resolution = resolve(&client, &manifest, &options, &cwd)
        .await
        .context("resolve (PubGrub)")?;
    let lock = resolution.to_lock(&manifest);
    let installer_paths = manifest.installer_paths();

    info(&format!(
        "Resolved {} prod + {} dev package(s) via PubGrub",
        lock.packages.len(),
        lock.packages_dev.len()
    ));

    if args.dry_run {
        for p in lock.packages_to_install(with_dev) {
            let dest = installer_paths
                .resolve_relative(&p.name, p.package_type.as_deref())
                .unwrap_or_else(|| format!("vendor/{}", p.name));
            println!("  - {} ({}) → {dest}", p.name, p.version);
        }
        success("Dry run complete");
        return Ok(());
    }

    lock.save(&lock_path)?;
    success(&format!("Wrote {}", lock_path.display()));

    std::fs::create_dir_all(&vendor)?;
    let installer = PackageInstaller::new(concurrency, args.verify_checksums)?
        .with_project_root(&cwd)
        .with_installer_paths(installer_paths);
    let packages = composer_resolver::locked_list(&lock, with_dev);
    let refs: Vec<_> = packages.iter().collect();
    installer.install_all(&refs, &vendor).await?;

    let stats = installer.stats().snapshot();
    info(&format!(
        "cache hits: {}  downloads: {}  hardlinks: {}",
        stats.cache_hits, stats.downloaded, stats.hardlinks
    ));

    generate(
        &cwd,
        &vendor,
        &manifest,
        Some(&lock),
        &AutoloadOptions {
            optimize: args.optimize_autoloader,
            classmap_authoritative: false,
            with_dev,
        },
    )?;

    success(&format!(
        "Update complete in {}",
        format_duration(start.elapsed())
    ));
    Ok(())
}
