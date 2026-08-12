//! `composer-rs global` — run a command against the global Composer home project.

use super::{info, success};
use anyhow::{Context, Result, bail};
use clap::Args;
use std::path::PathBuf;
use std::process::Command;

#[derive(Args, Debug, Clone)]
pub struct GlobalArgs {
    /// Arguments forwarded to composer-rs (e.g. `require phpunit/phpunit`)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Global project directory (`$COMPOSER_HOME` or XDG config `composer`).
pub fn global_home() -> PathBuf {
    if let Ok(h) = std::env::var("COMPOSER_HOME") {
        return PathBuf::from(h);
    }
    directories::BaseDirs::new()
        .map(|d| d.config_dir().join("composer"))
        .unwrap_or_else(|| PathBuf::from(".composer"))
}

pub fn run(args: GlobalArgs) -> Result<()> {
    if args.args.is_empty() {
        let home = global_home();
        println!("{}", home.display());
        info(
            "Usage: composer-rs global <command> [args...]  (e.g. global require phpunit/phpunit)",
        );
        return Ok(());
    }

    let home = global_home();
    std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;

    // Ensure a minimal composer.json exists for require/update.
    let json = home.join("composer.json");
    if !json.exists() {
        let skeleton = serde_json::json!({
            "require": {},
            "config": { "sort-packages": true }
        });
        std::fs::write(&json, serde_json::to_string_pretty(&skeleton)? + "\n")?;
        info(&format!("Created {}", json.display()));
    }

    let exe = std::env::current_exe().context("current_exe")?;
    let status = Command::new(&exe)
        .args(&args.args)
        .current_dir(&home)
        .env("COMPOSER_HOME", &home)
        .status()
        .with_context(|| format!("failed to run {} in {}", exe.display(), home.display()))?;

    if !status.success() {
        bail!(
            "global command failed with status {}",
            status.code().unwrap_or(1)
        );
    }
    success(&format!("global: {}", args.args.join(" ")));
    Ok(())
}
