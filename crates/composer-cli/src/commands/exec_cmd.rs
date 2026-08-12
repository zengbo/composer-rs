//! `composer-rs exec` — run a binary with vendor/bin on PATH.

use super::{project_paths, vendor_dir};
use anyhow::{Context, Result, bail};
use clap::Args;
use composer_manifest::ComposerJson;
use std::process::Command;

#[derive(Args, Debug, Clone)]
pub struct ExecArgs {
    /// Binary name or path
    pub binary: String,

    /// Arguments passed to the binary
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

pub fn run(args: ExecArgs) -> Result<()> {
    let (cwd, json_path, _) = project_paths()?;
    let bin_dir = if json_path.exists() {
        let manifest = ComposerJson::load(&json_path)?;
        let _ = vendor_dir(&manifest, &cwd);
        cwd.join(manifest.bin_dir())
    } else {
        cwd.join("vendor/bin")
    };

    let path_sep = if cfg!(windows) { ";" } else { ":" };
    let path_var = {
        let mut paths = Vec::new();
        if bin_dir.is_dir() {
            paths.push(bin_dir.display().to_string());
        }
        if let Ok(p) = std::env::var("PATH") {
            paths.push(p);
        }
        paths.join(path_sep)
    };

    // Prefer vendor/bin/<name> if present
    let program = {
        let candidate = bin_dir.join(&args.binary);
        if candidate.is_file() || candidate.is_symlink() {
            candidate
        } else {
            std::path::PathBuf::from(&args.binary)
        }
    };

    let status = Command::new(&program)
        .args(&args.args)
        .current_dir(&cwd)
        .env("PATH", path_var)
        .status()
        .with_context(|| format!("failed to spawn {}", program.display()))?;

    if !status.success() {
        let code = status.code().unwrap_or(1);
        bail!("command exited with status {code}");
    }
    Ok(())
}
