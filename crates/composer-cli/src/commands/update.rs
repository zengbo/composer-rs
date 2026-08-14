//! `composer-rs update`

use super::{format_duration, header, info, project_paths, success, vendor_dir};
use crate::hooks::{link_bins, run_lifecycle, warn_unapproved_plugins};
use anyhow::{Context, Result, bail};
use clap::Args;
use composer_auth::AuthStore;
use composer_autoload::{AutoloadOptions, generate};
use composer_core::{Platform, check_requirements_filtered};
use composer_download::{PackageInstaller, default_concurrency};
use composer_manifest::ComposerJson;
use composer_resolver::{ResolveOptions, UpdateDeps, resolve};
use composer_scripts::ScriptEvent;
use std::time::Instant;

#[derive(Args, Debug, Clone)]
pub struct UpdateArgs {
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

    #[arg(long = "ignore-platform-req", value_name = "REQ")]
    pub ignore_platform_req: Vec<String>,

    #[arg(long, default_value_t = true)]
    pub prefer_dist: bool,

    #[arg(long)]
    pub prefer_source: bool,

    #[arg(short = 'w', long = "with-dependencies")]
    pub with_dependencies: bool,

    #[arg(short = 'W', long = "with-all-dependencies")]
    pub with_all_dependencies: bool,

    #[arg(long)]
    pub no_autoloader: bool,

    #[arg(long)]
    pub no_scripts: bool,

    /// Only refresh lock content-hash / metadata without resolving versions
    #[arg(long)]
    pub lock: bool,

    #[arg(long)]
    pub audit: bool,
}

pub async fn run(args: UpdateArgs) -> Result<()> {
    let start = Instant::now();
    header("Updating dependencies");

    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }

    let composer_json_bytes =
        std::fs::read(&json_path).with_context(|| format!("read {}", json_path.display()))?;
    let manifest = ComposerJson::from_str(std::str::from_utf8(&composer_json_bytes)?)?;
    let vendor = vendor_dir(&manifest, &cwd);
    let concurrency = args.concurrency.unwrap_or_else(default_concurrency);
    let with_dev = !args.no_dev;

    if args.lock {
        if !lock_path.exists() {
            bail!("composer.lock not found");
        }
        let mut lock = composer_lock::ComposerLock::load(&lock_path)?;
        lock.content_hash = composer_lock::content_hash_from_composer_json(&composer_json_bytes)?;
        lock.save(&lock_path)?;
        success("Updated content-hash in composer.lock (--lock)");
        return Ok(());
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
            info("No composer.lock — cannot pin other packages; resolving full dependency tree");
        }
    }

    if !args.dry_run {
        run_lifecycle(
            &manifest,
            ScriptEvent::PreUpdateCmd,
            &cwd,
            args.no_scripts,
            with_dev,
        )?;
    }

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
        ignore_platform_req: args.ignore_platform_req.clone(),
        packages_to_update: args.packages.clone(),
        update_deps,
    };

    let resolution = resolve(&manifest, &options, &cwd, existing_lock.as_ref())
        .await
        .context("resolve (PubGrub)")?;
    let lock = resolution.to_lock(&manifest, &composer_json_bytes);
    let installer_paths = manifest.installer_paths();

    info(&format!(
        "Resolved {} prod + {} dev package(s) via PubGrub",
        lock.packages.len(),
        lock.packages_dev.len()
    ));

    if !args.ignore_platform_reqs {
        let mut platform = Platform::detect().context("detect PHP platform")?;
        platform.apply_config_platform(manifest.config.as_ref());
        let ign = &args.ignore_platform_req;
        check_requirements_filtered(&platform, &manifest.require, ign).context("platform check")?;
        if with_dev {
            check_requirements_filtered(&platform, &manifest.require_dev, ign)
                .context("platform check (dev)")?;
        }
        for pkg in lock.packages_to_install(with_dev) {
            check_requirements_filtered(&platform, &pkg.require, ign)
                .with_context(|| format!("platform check for {}", pkg.name))?;
        }
        if platform.reliable {
            info(&format!(
                "Platform OK (PHP {})",
                platform.php_version().as_str()
            ));
        }
    }

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

    warn_unapproved_plugins(&lock, &manifest, with_dev);

    lock.save(&lock_path)?;
    success(&format!("Wrote {}", lock_path.display()));

    std::fs::create_dir_all(&vendor)?;
    let prefer_dist = manifest.resolve_prefer_dist(args.prefer_dist, args.prefer_source);
    let auth = AuthStore::load(Some(&cwd)).unwrap_or_default();
    let installer = PackageInstaller::new(concurrency, args.verify_checksums)?
        .with_project_root(&cwd)
        .with_installer_paths(installer_paths.clone())
        .with_prefer_dist(prefer_dist)
        .with_secure_http(manifest.secure_http())
        .with_auth(auth);
    let packages = composer_resolver::locked_list(&lock, with_dev);
    let refs: Vec<_> = packages.iter().collect();
    installer.install_all(&refs, &vendor).await?;

    let stats = installer.stats().snapshot();
    info(&format!(
        "cache hits: {}  downloads: {}  skipped: {}  hardlinks: {}  copies: {}",
        stats.cache_hits, stats.downloaded, stats.skipped, stats.hardlinks, stats.copies,
    ));
    super::warn_copy_install(stats.copies);

    link_bins(&refs, &vendor, &cwd, &manifest, &installer_paths)?;

    if !args.no_autoloader {
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
    }

    run_lifecycle(
        &manifest,
        ScriptEvent::PostUpdateCmd,
        &cwd,
        args.no_scripts,
        with_dev,
    )?;

    success(&format!(
        "Update complete in {}",
        format_duration(start.elapsed())
    ));

    // Composer-compatible: --audit fails the command when advisories are found.
    if args.audit {
        let audit_args = super::audit::AuditArgs {
            format: "table".into(),
            no_dev: args.no_dev,
        };
        super::audit::run(audit_args)
            .await
            .context("security audit failed")?;
    }

    Ok(())
}
