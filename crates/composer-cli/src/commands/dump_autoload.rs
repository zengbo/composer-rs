//! `composer-rs dump-autoload`

use super::{header, project_paths, success, vendor_dir};
use anyhow::{bail, Result};
use clap::Args;
use composer_autoload::{generate, AutoloadOptions};
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;

#[derive(Args, Debug, Clone)]
pub struct DumpAutoloadArgs {
    #[arg(short = 'o', long)]
    pub optimize: bool,

    #[arg(short = 'a', long)]
    pub classmap_authoritative: bool,

    #[arg(long)]
    pub no_dev: bool,
}

pub fn run(args: DumpAutoloadArgs) -> Result<()> {
    header("Generating autoload files");
    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    let manifest = ComposerJson::load(&json_path)?;
    let vendor = vendor_dir(&manifest, &cwd);
    let lock = if lock_path.exists() {
        Some(ComposerLock::load(&lock_path)?)
    } else {
        None
    };

    generate(
        &cwd,
        &vendor,
        &manifest,
        lock.as_ref(),
        &AutoloadOptions {
            optimize: args.optimize,
            classmap_authoritative: args.classmap_authoritative,
            with_dev: !args.no_dev,
        },
    )?;
    success("Autoloader dumped");
    Ok(())
}
