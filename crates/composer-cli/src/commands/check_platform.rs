//! `composer-rs check-platform-reqs`

use super::{info, project_paths, success};
use anyhow::{Result, bail};
use clap::Args;
use composer_core::{Platform, check_requirements_filtered};
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;

#[derive(Args, Debug, Clone)]
pub struct CheckPlatformArgs {
    #[arg(long)]
    pub no_dev: bool,

    /// Only check lock packages (skip root require)
    #[arg(long)]
    pub lock: bool,

    #[arg(long = "ignore-platform-req", value_name = "REQ")]
    pub ignore_platform_req: Vec<String>,
}

pub fn run(args: CheckPlatformArgs) -> Result<()> {
    let (cwd, json_path, lock_path) = project_paths()?;
    let _ = cwd;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    let manifest = ComposerJson::load(&json_path)?;
    let platform = Platform::detect()?;
    // Real platform: ignore config.platform for this command (Composer behavior).

    let with_dev = !args.no_dev;
    let ignore = &args.ignore_platform_req;

    if !args.lock {
        check_requirements_filtered(&platform, &manifest.require, ignore)?;
        if with_dev {
            check_requirements_filtered(&platform, &manifest.require_dev, ignore)?;
        }
    }

    if lock_path.exists() {
        let lock = ComposerLock::load(&lock_path)?;
        for pkg in lock.packages_to_install(with_dev) {
            check_requirements_filtered(&platform, &pkg.require, ignore)
                .map_err(|e| anyhow::anyhow!("{}: {e}", pkg.name))?;
        }
    }

    if platform.reliable {
        success(&format!(
            "Platform requirements OK (PHP {})",
            platform.php_version().as_str()
        ));
    } else {
        info("Platform check skipped (PHP not detected)");
    }
    Ok(())
}
