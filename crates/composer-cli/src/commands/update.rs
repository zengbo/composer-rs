//! `composer-rs update`

use super::{format_duration, header, info, project_paths, success, vendor_dir};
use anyhow::{bail, Context, Result};
use clap::Args;
use composer_autoload::{generate, AutoloadOptions};
use composer_download::{default_concurrency, PackageInstaller};
use composer_manifest::ComposerJson;
use composer_core::{check_requirements, Platform};
use composer_resolver::{resolve, ResolveOptions, UpdateDeps};
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

    #[arg(short = 'a', long)]
    pub classmap_authoritative: bool,

    #[arg(long)]
    pub concurrency: Option<usize>,

    #[arg(long)]
    pub verify_checksums: bool,

    #[arg(long)]
    pub ignore_platform_reqs: bool,

    /// Prefer dist archives (default)
    #[arg(long, default_value_t = true)]
    pub prefer_dist: bool,

    /// Prefer VCS source installs over dist archives
    #[arg(long)]
    pub prefer_source: bool,

    /// Also update dependencies of listed packages, except root requirements
    #[arg(short = 'w', long = "with-dependencies")]
    pub with_dependencies: bool,

    /// Also update dependencies of listed packages, including root requirements
    #[arg(short = 'W', long = "with-all-dependencies")]
    pub with_all_dependencies: bool,
}

pub async fn run(args: UpdateArgs) -> Result<()> {
    let start = Instant::now();
    header("Updating dependencies");

    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    let update_deps = if args.with_all_dependencies {
        UpdateDeps::WithAllDependencies
    } else if args.with_dependencies {
        UpdateDeps::WithDependencies
    } else {
        UpdateDeps::OnlyListed
    };

    if !args.packages.is_empty() {
        if lock_path.exists() {
            let scope = match update_deps {
                UpdateDeps::OnlyListed => "listed packages only",
                UpdateDeps::WithDependencies => "listed + non-root dependencies (-w)",
                UpdateDeps::WithAllDependencies => "listed + all dependencies (-W)",
            };
            info(&format!(
                "Partial update: {} package(s) ({scope}); others pinned to lock",
                args.packages.len()
            ));
        } else {
            info(
                "No composer.lock — cannot pin other packages; resolving full dependency tree",
            );
        }
    }

    let manifest = ComposerJson::load(&json_path)?;
    let vendor = vendor_dir(&manifest, &cwd);
    let concurrency = args.concurrency.unwrap_or_else(default_concurrency);
    let with_dev = !args.no_dev;

    let existing_lock = if lock_path.exists() && !args.packages.is_empty() {
        Some(composer_lock::ComposerLock::load(&lock_path)?)
    } else {
        None
    };

    let options = ResolveOptions {
        with_dev,
        prefer_stable: args.prefer_stable || manifest.prefer_stable(),
        prefer_lowest: args.prefer_lowest,
        minimum_stability: manifest.minimum_stability().to_string(),
        concurrency,
        ignore_platform_reqs: args.ignore_platform_reqs,
        packages_to_update: args.packages.clone(),
        update_deps,
    };

    let resolution = resolve(
        &manifest,
        &options,
        &cwd,
        existing_lock.as_ref(),
    )
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

    if !args.ignore_platform_reqs {
        let mut platform = Platform::detect().context("detect PHP platform")?;
        platform.apply_config_platform(manifest.config.as_ref());
        check_requirements(&platform, &manifest.require).context("platform check")?;
        if with_dev {
            check_requirements(&platform, &manifest.require_dev).context("platform check (dev)")?;
        }
        for pkg in lock.packages_to_install(with_dev) {
            check_requirements(&platform, &pkg.require)
                .with_context(|| format!("platform check for {}", pkg.name))?;
        }
        if platform.reliable {
            info(&format!(
                "Platform OK (PHP {})",
                platform.php_version().as_str()
            ));
        }
    }

    lock.save(&lock_path)?;
    success(&format!("Wrote {}", lock_path.display()));

    std::fs::create_dir_all(&vendor)?;
    let prefer_dist = args.prefer_dist && !args.prefer_source;
    let installer = PackageInstaller::new(concurrency, args.verify_checksums)?
        .with_project_root(&cwd)
        .with_installer_paths(installer_paths)
        .with_prefer_dist(prefer_dist);
    let packages = composer_resolver::locked_list(&lock, with_dev);
    let refs: Vec<_> = packages.iter().collect();
    installer.install_all(&refs, &vendor).await?;

    let stats = installer.stats().snapshot();
    info(&format!(
        "cache hits: {}  downloads: {}  skipped: {}  hardlinks: {}  copies: {}",
        stats.cache_hits,
        stats.downloaded,
        stats.skipped,
        stats.hardlinks,
        stats.copies,
    ));

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

    success(&format!(
        "Update complete in {}",
        format_duration(start.elapsed())
    ));
    Ok(())
}
