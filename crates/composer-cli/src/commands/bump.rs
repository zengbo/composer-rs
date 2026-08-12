//! `composer-rs bump` — raise lower bounds in composer.json to locked versions.

use super::{info, project_paths, success};
use anyhow::{Result, bail};
use clap::Args;
use composer_core::ComposerVersion;
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;

#[derive(Args, Debug, Clone)]
pub struct BumpArgs {
    /// Only bump require-dev
    #[arg(long)]
    pub dev_only: bool,

    /// Only bump require (not require-dev)
    #[arg(long)]
    pub no_dev_only: bool,

    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: BumpArgs) -> Result<()> {
    let (_cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    if !lock_path.exists() {
        bail!("composer.lock not found — nothing to bump against");
    }
    let mut manifest = ComposerJson::load(&json_path)?;
    let lock = ComposerLock::load(&lock_path)?;

    let mut changed = 0usize;
    if !args.dev_only {
        changed += bump_map(&mut manifest.require, &lock, args.dry_run);
    }
    if !args.no_dev_only {
        changed += bump_map(&mut manifest.require_dev, &lock, args.dry_run);
    }

    if changed == 0 {
        info("Nothing to bump (constraints already match lock or no locked packages)");
        return Ok(());
    }

    if args.dry_run {
        success(&format!("Would bump {changed} constraint(s) (dry-run)"));
        return Ok(());
    }

    manifest.save(&json_path)?;
    success(&format!("Bumped {changed} constraint(s) in composer.json"));
    Ok(())
}

fn bump_map(
    map: &mut std::collections::BTreeMap<String, String>,
    lock: &ComposerLock,
    dry_run: bool,
) -> usize {
    let mut n = 0;
    for (name, constraint) in map.iter_mut() {
        if name == "php" || name.starts_with("ext-") || name.starts_with("lib-") {
            continue;
        }
        let Some(pkg) = lock.find(name) else {
            continue;
        };
        let ver = pkg.version.trim_start_matches('v');
        // Skip branch-like versions
        if ver.starts_with("dev-") || ComposerVersion::parse(ver).is_err() {
            continue;
        }
        let bumped = format!("^{ver}");
        if *constraint == bumped {
            continue;
        }
        // Don't bump if constraint already pins exactly
        if constraint == ver || constraint == &format!("={ver}") {
            continue;
        }
        if dry_run {
            println!("  {name}: {constraint} → {bumped}");
        } else {
            info(&format!("{name}: {constraint} → {bumped}"));
            *constraint = bumped;
        }
        n += 1;
    }
    n
}
