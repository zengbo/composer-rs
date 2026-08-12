//! `composer-rs validate`

use super::{header, project_paths, success, warning};
use anyhow::{bail, Result};
use clap::Args;
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;

#[derive(Args, Debug, Clone)]
pub struct ValidateArgs {
    /// Do not check composer.lock
    #[arg(long)]
    pub no_check_lock: bool,
}

pub fn run(args: ValidateArgs) -> Result<()> {
    header("Validating composer.json");
    let (_, json_path, lock_path) = project_paths()?;

    if !json_path.exists() {
        bail!("composer.json not found");
    }

    let manifest = ComposerJson::load(&json_path)?;
    if let Some(name) = &manifest.name {
        if !name.contains('/') {
            warning(&format!("name '{name}' should be vendor/package"));
        }
    } else {
        warning("name is missing");
    }

    success("composer.json is valid");

    if !args.no_check_lock && lock_path.exists() {
        let _lock = ComposerLock::load(&lock_path)?;
        success("composer.lock is valid JSON");
    }

    Ok(())
}
