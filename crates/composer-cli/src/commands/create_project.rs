//! `composer-rs create-project`

use super::{format_duration, header, info, success, warning};
use anyhow::{Context, Result, bail};
use clap::Args;
use composer_auth::AuthStore;
use composer_download::{PackageInstaller, default_concurrency, install_bins};
use composer_manifest::ComposerJson;
use composer_resolver::{ResolveOptions, locked_list, resolve};
use composer_scripts::{ScriptEvent, run_event};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Args, Debug, Clone)]
pub struct CreateProjectArgs {
    /// Package name (vendor/package)
    pub package: String,

    /// Target directory (default: package name suffix)
    pub directory: Option<PathBuf>,

    /// Version constraint (default: *)
    pub version: Option<String>,

    /// Do not run install after scaffolding
    #[arg(long)]
    pub no_install: bool,

    #[arg(long)]
    pub no_scripts: bool,

    #[arg(long)]
    pub ignore_platform_reqs: bool,

    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: CreateProjectArgs) -> Result<()> {
    let start = Instant::now();
    header("Creating project");

    let dir_name = args
        .directory
        .clone()
        .unwrap_or_else(|| PathBuf::from(args.package.split('/').next_back().unwrap_or("project")));
    if dir_name.exists() {
        bail!("target directory already exists: {}", dir_name.display());
    }
    std::fs::create_dir_all(&dir_name)?;

    let version = args.version.as_deref().unwrap_or("*");
    // Minimal composer.json requiring the package, then resolve & install into dir.
    let mut require = serde_json::Map::new();
    require.insert("php".into(), serde_json::json!(">=8.0"));
    require.insert(args.package.clone(), serde_json::json!(version));
    let manifest_json = serde_json::json!({
        "name": "created-project/root",
        "require": require,
        "config": {
            "platform": { "php": "8.2.0" }
        }
    });
    let json_path = dir_name.join("composer.json");
    std::fs::write(
        &dir_name.join("composer.json"),
        serde_json::to_string_pretty(&manifest_json)? + "\n",
    )?;

    if args.no_install {
        success(&format!("Created {} (no-install)", dir_name.display()));
        return Ok(());
    }

    let manifest = ComposerJson::load(&json_path)?;
    let concurrency = args.concurrency.unwrap_or_else(default_concurrency);
    let options = ResolveOptions {
        with_dev: true,
        prefer_stable: true,
        prefer_lowest: false,
        minimum_stability: "stable".into(),
        concurrency,
        ignore_platform_reqs: args.ignore_platform_reqs,
        ignore_platform_req: Vec::new(),
        packages_to_update: Vec::new(),
        update_deps: Default::default(),
    };
    let resolution = resolve(&manifest, &options, &dir_name, None)
        .await
        .context("resolve project package")?;
    let lock = resolution.to_lock(&manifest);
    lock.save(&dir_name.join("composer.lock"))?;

    let vendor = dir_name.join(manifest.vendor_dir());
    std::fs::create_dir_all(&vendor)?;
    let auth = AuthStore::load(Some(&dir_name)).unwrap_or_default();
    let installer = PackageInstaller::new(concurrency, false)?
        .with_project_root(&dir_name)
        .with_installer_paths(manifest.installer_paths())
        .with_auth(auth);
    let packages = locked_list(&lock, true);
    let refs: Vec<_> = packages.iter().collect();
    installer.install_all(&refs, &vendor).await?;
    super::warn_copy_install(installer.stats().snapshot().copies);

    let bin_dir = dir_name.join(manifest.bin_dir());
    let _ = install_bins(
        &refs,
        &vendor,
        &dir_name,
        &bin_dir,
        &manifest.installer_paths(),
    );

    // type:project packages become the project root (Composer create-project semantics).
    if let Some(pkg) = lock.find(&args.package) {
        if pkg.package_type.as_deref() == Some("project") {
            let src = vendor.join(&pkg.name);
            if src.is_dir() {
                info(&format!(
                    "Unpacking project package {} into {}",
                    pkg.name,
                    dir_name.display()
                ));
                copy_project_root(&src, &dir_name)?;
                // Prefer the project's own composer.json / lock after unpack when present.
                if dir_name.join("composer.json").is_file() {
                    info("Using project package composer.json as root");
                }
            }
        }
    }

    // Reload manifest if unpack replaced composer.json
    let manifest = ComposerJson::load(&dir_name.join("composer.json")).unwrap_or(manifest);

    composer_autoload::generate(
        &dir_name,
        &vendor,
        &manifest,
        Some(&lock),
        &composer_autoload::AutoloadOptions {
            optimize: false,
            classmap_authoritative: false,
            with_dev: true,
        },
    )?;

    if !args.no_scripts {
        let _ = run_event(&manifest, ScriptEvent::PostInstallCmd, &dir_name, true);
    }

    success(&format!(
        "Project created in {} ({})",
        dir_name.display(),
        format_duration(start.elapsed())
    ));
    let _ = warning;
    Ok(())
}

/// Copy project package files into the target directory (does not wipe existing vendor/).
fn copy_project_root(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        if rel.as_os_str().is_empty() {
            continue;
        }
        // Keep our installed vendor/ tree
        if rel
            .components()
            .next()
            .is_some_and(|c| c.as_os_str() == "vendor")
        {
            continue;
        }
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target).with_context(|| {
                format!("copy {} → {}", entry.path().display(), target.display())
            })?;
        }
    }
    Ok(())
}
