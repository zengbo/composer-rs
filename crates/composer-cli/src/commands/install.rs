//! `composer-rs install`

use super::{
    format_duration, header, info, project_paths, success, vendor_dir, warn_copy_install, warning,
};
use crate::hooks::{link_bins, run_lifecycle, warn_unapproved_plugins};
use anyhow::{Context, Result, bail};
use clap::Args;
use composer_auth::AuthStore;
use composer_autoload::{AutoloadOptions, generate};
use composer_core::{Platform, check_requirements_filtered};
use composer_download::{PackageInstaller, default_concurrency};
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;
use composer_resolver::{ResolveOptions, locked_list, resolve};
use composer_scripts::ScriptEvent;
use std::time::Instant;

#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    #[arg(long)]
    pub no_dev: bool,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long, default_value_t = true)]
    pub prefer_dist: bool,

    #[arg(long)]
    pub prefer_source: bool,

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

    #[arg(long)]
    pub no_autoloader: bool,

    #[arg(long)]
    pub no_scripts: bool,

    #[arg(long)]
    pub audit: bool,
}

pub async fn run(args: InstallArgs) -> Result<()> {
    let start = Instant::now();
    header("Installing dependencies from lock file");

    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found in {}", cwd.display());
    }

    let composer_json_bytes =
        std::fs::read(&json_path).with_context(|| format!("read {}", json_path.display()))?;
    let manifest = ComposerJson::from_str(std::str::from_utf8(&composer_json_bytes)?)
        .context("parse composer.json")?;
    let vendor = vendor_dir(&manifest, &cwd);
    let with_dev = !args.no_dev;
    let concurrency = args.concurrency.unwrap_or_else(default_concurrency);

    run_lifecycle(
        &manifest,
        ScriptEvent::PreInstallCmd,
        &cwd,
        args.no_scripts,
        with_dev,
    )?;

    let lock = if lock_path.exists() {
        info(&format!("Lock file: {}", lock_path.display()));
        let lock = ComposerLock::load(&lock_path).context("parse composer.lock")?;
        let expected = composer_lock::content_hash_from_composer_json(&composer_json_bytes)?;
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
            ignore_platform_req: args.ignore_platform_req.clone(),
            packages_to_update: Vec::new(),
            update_deps: Default::default(),
        };
        let resolution = resolve(&manifest, &options, &cwd, None)
            .await
            .context("dependency resolution (PubGrub)")?;
        let lock = resolution.to_lock(&manifest, &composer_json_bytes);
        if !args.dry_run {
            lock.save(&lock_path)?;
            success(&format!("Wrote {}", lock_path.display()));
        }
        lock
    };

    let packages = locked_list(&lock, with_dev);
    let installer_paths = manifest.installer_paths();
    if manifest
        .extra
        .as_ref()
        .and_then(|e| e.get("installer-paths"))
        .is_some()
    {
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
        let ign = &args.ignore_platform_req;
        check_requirements_filtered(&platform, &manifest.require, ign).context("platform check")?;
        if with_dev {
            check_requirements_filtered(&platform, &manifest.require_dev, ign)
                .context("platform check (dev)")?;
        }
        for pkg in &packages {
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

    warn_unapproved_plugins(&lock, &manifest, with_dev);

    std::fs::create_dir_all(&vendor)?;
    let prefer_dist = manifest.resolve_prefer_dist(args.prefer_dist, args.prefer_source);
    let auth = AuthStore::load(Some(&cwd)).unwrap_or_default();
    let installer = PackageInstaller::new(concurrency, args.verify_checksums)?
        .with_project_root(&cwd)
        .with_installer_paths(installer_paths.clone())
        .with_prefer_dist(prefer_dist)
        .with_auth(auth);
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
    warn_copy_install(stats.copies);

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
        success("Autoloader generated");
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
        ScriptEvent::PostInstallCmd,
        &cwd,
        args.no_scripts,
        with_dev,
    )?;

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
