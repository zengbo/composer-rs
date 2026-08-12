//! `composer-rs remove`

use super::{header, project_paths, success};
use anyhow::{Result, bail};
use clap::Args;
use composer_manifest::ComposerJson;

#[derive(Args, Debug, Clone)]
pub struct RemoveArgs {
    pub packages: Vec<String>,

    #[arg(long)]
    pub no_update: bool,

    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: RemoveArgs) -> Result<()> {
    header("Remove package");
    if args.packages.is_empty() {
        bail!("specify package name(s) to remove");
    }

    let (_, json_path, _) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }

    let mut manifest = ComposerJson::load(&json_path)?;
    let mut removed = 0usize;
    for name in &args.packages {
        if manifest.require.remove(name).is_some() {
            removed += 1;
        }
        if manifest.require_dev.remove(name).is_some() {
            removed += 1;
        }
    }

    if removed == 0 {
        bail!("no matching packages in composer.json");
    }

    if args.dry_run {
        success("Dry run — composer.json not modified");
        return Ok(());
    }

    manifest.save(&json_path)?;
    success(&format!("Updated {}", json_path.display()));

    if !args.no_update {
        let update_args = super::update::UpdateArgs {
            packages: vec![],
            no_dev: false,
            prefer_lowest: false,
            prefer_stable: true,
            dry_run: false,
            optimize_autoloader: false,
            classmap_authoritative: false,
            concurrency: None,
            verify_checksums: false,
            ignore_platform_reqs: false,
            ignore_platform_req: Vec::new(),
            prefer_dist: true,
            prefer_source: false,
            with_dependencies: false,
            with_all_dependencies: false,
            no_autoloader: false,
            no_scripts: false,
            lock: false,
            audit: false,
        };
        super::update::run(update_args).await?;
    }

    Ok(())
}
