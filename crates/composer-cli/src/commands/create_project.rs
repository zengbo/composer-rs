//! `composer-rs create-project`

use super::{format_duration, header, info, success, vendor_dir};
use anyhow::{Context, Result, bail};
use clap::Args;
use composer_auth::AuthStore;
use composer_core::{PackageId, VersionConstraint};
use composer_download::{
    PackageInstaller, default_concurrency, extract_archive, install_bins,
    promote_extracted_package_root,
};
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;
use composer_repo::RepositoryRegistry;
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
    let id = PackageId::parse(&args.package).context("invalid package name")?;
    let constraint = VersionConstraint::new(version);
    let auth = AuthStore::load(None).unwrap_or_default();
    let bootstrap = ComposerJson::from_str("{}").context("empty manifest")?;
    let registry = RepositoryRegistry::from_manifest_auth(&bootstrap, auth.clone())
        .context("repository setup")?;
    let remote = registry
        .find_best(&id, &constraint, true, "stable")
        .await
        .with_context(|| format!("find project package {}", args.package))?;
    let locked = remote.to_locked();
    if locked.dist_url().is_none() {
        bail!(
            "package {} has no dist URL; create-project requires a downloadable archive",
            args.package
        );
    }

    let concurrency = args.concurrency.unwrap_or_else(default_concurrency);
    let installer = PackageInstaller::new(concurrency, false)?.with_auth(auth);
    let archive = installer
        .download_dist_archive(&locked)
        .await
        .context("download project package")?;
    extract_archive(&archive, &dir_name).context("extract project package")?;
    promote_extracted_package_root(&dir_name).context("unwrap package archive root")?;

    let json_path = dir_name.join("composer.json");
    if !json_path.is_file() {
        bail!(
            "extracted package {} has no composer.json in {}",
            args.package,
            dir_name.display()
        );
    }
    info(&format!(
        "Created project from {} ({})",
        locked.name, locked.version
    ));

    if args.no_install {
        success(&format!("Created {} (no-install)", dir_name.display()));
        return Ok(());
    }

    let composer_json_bytes =
        std::fs::read(&json_path).with_context(|| format!("read {}", json_path.display()))?;
    let manifest = ComposerJson::from_str(std::str::from_utf8(&composer_json_bytes)?)
        .context("parse project composer.json")?;
    let lock_path = dir_name.join("composer.lock");
    let lock = if lock_path.is_file() {
        info("Using lock file shipped with the project package");
        ComposerLock::load(&lock_path).context("parse project composer.lock")?
    } else {
        let options = ResolveOptions {
            with_dev: true,
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: manifest.minimum_stability().to_string(),
            concurrency,
            ignore_platform_reqs: args.ignore_platform_reqs,
            ignore_platform_req: Vec::new(),
            packages_to_update: Vec::new(),
            update_deps: Default::default(),
        };
        let resolution = resolve(&manifest, &options, &dir_name, None)
            .await
            .context("resolve project dependencies")?;
        let lock = resolution.to_lock(&manifest, &composer_json_bytes);
        lock.save(&lock_path)?;
        success(&format!("Wrote {}", lock_path.display()));
        lock
    };

    let vendor = vendor_dir(&manifest, &dir_name);
    std::fs::create_dir_all(&vendor)?;
    let project_auth = AuthStore::load(Some(&dir_name)).unwrap_or_default();
    let installer = PackageInstaller::new(concurrency, false)?
        .with_project_root(&dir_name)
        .with_installer_paths(manifest.installer_paths())
        .with_secure_http(manifest.secure_http())
        .with_auth(project_auth);
    let packages = locked_list(&lock, true);
    let refs: Vec<_> = packages.iter().collect();
    installer
        .install_all(&refs, &vendor)
        .await
        .context("install project dependencies")?;
    super::warn_copy_install(installer.stats().snapshot().copies);

    let bin_dir = dir_name.join(manifest.bin_dir());
    install_bins(
        &refs,
        &vendor,
        &dir_name,
        &bin_dir,
        &manifest.installer_paths(),
    )
    .context("install package binaries")?;

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
        run_event(&manifest, ScriptEvent::PostInstallCmd, &dir_name, true)
            .context("post-install-cmd")?;
    }

    success(&format!(
        "Project created in {} ({})",
        dir_name.display(),
        format_duration(start.elapsed())
    ));
    Ok(())
}
