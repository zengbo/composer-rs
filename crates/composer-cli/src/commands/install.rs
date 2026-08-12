//! `composer-rs install`

use super::{format_duration, header, info, project_paths, success, vendor_dir, warning};
use anyhow::{bail, Context, Result};
use clap::Args;
use composer_autoload::{generate, AutoloadOptions};
use composer_download::{default_concurrency, PackageInstaller};
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;
use composer_core::{check_requirements, Platform};
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

    /// Prefer VCS source installs over dist archives
    #[arg(long)]
    pub prefer_source: bool,

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

    /// Ignore platform requirements (php, ext-*)
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
        let lock = ComposerLock::load(&lock_path).context("parse composer.lock")?;
        let expected = composer_lock::content_hash_from_relevant(
            &composer_manifest::relevant_content(&manifest),
        );
        if !lock.content_hash.is_empty() && lock.content_hash != expected {
            warning(&format!(
                "composer.lock content-hash mismatch (lock={}, computed={expected}) — run update",
                lock.content_hash
            ));
        }
        lock
    } else {
        warning("No composer.lock found — resolving from composer.json");
        let options = ResolveOptions {
            with_dev,
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: manifest.minimum_stability().to_string(),
            concurrency,
            ignore_platform_reqs: args.ignore_platform_reqs,
            packages_to_update: Vec::new(),
            update_deps: Default::default(),
        };
        let resolution = resolve(&manifest, &options, &cwd, None)
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

    if !args.ignore_platform_reqs {
        let mut platform = Platform::detect().context("detect PHP platform")?;
        platform.apply_config_platform(manifest.config.as_ref());
        check_requirements(&platform, &manifest.require).context("platform check")?;
        if with_dev {
            check_requirements(&platform, &manifest.require_dev).context("platform check (dev)")?;
        }
        for pkg in &packages {
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

    std::fs::create_dir_all(&vendor)?;

    let prefer_dist = args.prefer_dist && !args.prefer_source;
    let installer = PackageInstaller::new(concurrency, args.verify_checksums)?
        .with_project_root(&cwd)
        .with_installer_paths(installer_paths)
        .with_prefer_dist(prefer_dist);
    let refs: Vec<&composer_lock::LockedPackage> = packages.iter().collect();
    installer
        .install_all(&refs, &vendor)
        .await
        .context("install packages")?;

    let stats = installer.stats().snapshot();
    info(&format!(
        "cache hits: {}  downloads: {}  skipped: {}  hardlinks: {}  copies: {}  bytes: {}",
        stats.cache_hits,
        stats.downloaded,
        stats.skipped,
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
