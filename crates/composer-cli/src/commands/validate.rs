//! `composer-rs validate`

use super::{header, project_paths, success, warning};
use anyhow::{Result, bail};
use clap::Args;
use composer_core::{PackageId, VersionConstraint};
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;

#[derive(Args, Debug, Clone)]
pub struct ValidateArgs {
    #[arg(long)]
    pub no_check_lock: bool,

    #[arg(long)]
    pub strict: bool,
}

pub fn run(args: ValidateArgs) -> Result<()> {
    header("Validating composer.json");
    let (_, json_path, lock_path) = project_paths()?;

    if !json_path.exists() {
        bail!("composer.json not found");
    }

    let manifest = ComposerJson::load(&json_path)?;
    let mut warnings = 0usize;

    if let Some(name) = &manifest.name {
        if !name.contains('/') {
            warning(&format!("name '{name}' should be vendor/package"));
            warnings += 1;
        }
    } else {
        warning("name is missing");
        warnings += 1;
    }

    for (map_name, map) in [
        ("require", &manifest.require),
        ("require-dev", &manifest.require_dev),
    ] {
        for (pkg, constraint) in map {
            if PackageId::parse(pkg).is_err() {
                warning(&format!("{map_name}: invalid package name `{pkg}`"));
                warnings += 1;
            }
            // empty constraint is invalid
            if constraint.trim().is_empty() {
                warning(&format!("{map_name}.{pkg}: empty version constraint"));
                warnings += 1;
            } else {
                // ensure constraint parser accepts it (best-effort)
                let _ = VersionConstraint::new(constraint.clone());
            }
        }
    }

    // repositories shape
    if let Some(repos) = &manifest.repositories {
        if !repos.is_array() && !repos.is_object() {
            warning("repositories must be an array or object");
            warnings += 1;
        }
    }

    success("composer.json is valid");

    if !args.no_check_lock && lock_path.exists() {
        let lock = ComposerLock::load(&lock_path)?;
        success("composer.lock is valid JSON");
        let expected = composer_lock::content_hash_from_relevant(
            &composer_manifest::relevant_content(&manifest),
        );
        if lock.content_hash.is_empty() {
            warning("composer.lock is missing content-hash");
            warnings += 1;
        } else if lock.content_hash != expected {
            warning(&format!(
                "content-hash mismatch: lock has {} but composer.json computes {expected}",
                lock.content_hash
            ));
            warnings += 1;
        } else {
            success("content-hash matches composer.json");
        }
    }

    if args.strict && warnings > 0 {
        bail!("{warnings} warning(s) treated as errors (--strict)");
    }
    Ok(())
}
