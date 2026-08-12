pub mod cache;
pub mod dump_autoload;
pub mod init_cmd;
pub mod install;
pub mod remove;
pub mod require;
pub mod search;
pub mod show;
pub mod update;
pub mod validate;

use console::style;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn project_paths() -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let composer_json = cwd.join("composer.json");
    let composer_lock = cwd.join("composer.lock");
    Ok((cwd, composer_json, composer_lock))
}

pub fn vendor_dir(manifest: &composer_manifest::ComposerJson, cwd: &Path) -> PathBuf {
    cwd.join(manifest.vendor_dir())
}

pub fn header(msg: &str) {
    println!("{}", style(msg).cyan().bold());
}

pub fn success(msg: &str) {
    println!("{} {}", style("✓").green().bold(), msg);
}

pub fn info(msg: &str) {
    println!("{} {}", style("i").blue().bold(), msg);
}

pub fn warning(msg: &str) {
    println!("{} {}", style("!").yellow().bold(), msg);
}

pub fn format_duration(d: Duration) -> String {
    if d.as_secs() >= 60 {
        format!("{}m {:02}s", d.as_secs() / 60, d.as_secs() % 60)
    } else if d.as_secs() >= 1 {
        format!("{:.2}s", d.as_secs_f64())
    } else {
        format!("{}ms", d.as_millis())
    }
}
