//! `composer-rs show`

use super::{header, project_paths, success};
use anyhow::{bail, Result};
use clap::Args;
use composer_core::PackageId;
use composer_lock::ComposerLock;
use composer_repo::RepositoryClient;

#[derive(Args, Debug, Clone)]
pub struct ShowArgs {
    /// Package name (omit to list installed)
    pub package: Option<String>,

    /// Show all available versions from Packagist
    #[arg(long)]
    pub all: bool,
}

pub async fn run(args: ShowArgs) -> Result<()> {
    let (_, _, lock_path) = project_paths()?;

    if let Some(name) = &args.package {
        header(&format!("Package {name}"));
        let id = PackageId::parse(name)?;
        let client = RepositoryClient::new()?;
        let versions = client.show(&id).await?;
        if versions.is_empty() {
            bail!("package not found");
        }

        if args.all {
            for v in &versions {
                println!("  {}", v.version.raw);
            }
        } else {
            let latest = versions
                .iter()
                .filter(|v| v.version.is_stable())
                .max_by(|a, b| a.version.cmp(&b.version))
                .or_else(|| versions.first())
                .unwrap();
            println!("name       : {}", latest.name);
            println!("version    : {}", latest.version.raw);
            println!(
                "type       : {}",
                latest.package_type.as_deref().unwrap_or("library")
            );
            if let Some(d) = &latest.description {
                println!("description: {d}");
            }
            if let Some(dist) = &latest.dist {
                println!("dist       : {}", dist.url);
            }
            if !latest.require.is_empty() {
                println!("requires:");
                for (k, v) in &latest.require {
                    println!("  {k}: {v}");
                }
            }
        }
        success("done");
        return Ok(());
    }

    // List installed from lock
    header("Installed packages");
    if !lock_path.exists() {
        bail!("no composer.lock — nothing installed to show");
    }
    let lock = ComposerLock::load(&lock_path)?;
    for p in lock.packages_to_install(true) {
        println!("  {}  {}", p.name, p.version);
    }
    success(&format!(
        "{} package(s)",
        lock.packages.len() + lock.packages_dev.len()
    ));
    Ok(())
}
