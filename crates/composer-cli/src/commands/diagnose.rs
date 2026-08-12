//! `composer-rs diagnose`

use super::{info, project_paths, success, warning};
use anyhow::Result;
use clap::Args;
use composer_auth::{AuthStore, global_auth_path};
use composer_cache::cache_root;
use std::process::Command;

#[derive(Args, Debug, Clone)]
pub struct DiagnoseArgs {}

pub async fn run(_args: DiagnoseArgs) -> Result<()> {
    let mut ok = true;
    let (cwd, json_path, _) = project_paths()?;

    // PHP
    match Command::new("php").arg("-v").output() {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            let line = v.lines().next().unwrap_or("php ok");
            success(&format!("PHP: {line}"));
        }
        _ => {
            warning("PHP: not found on PATH (set COMPOSER_PLATFORM_PHP or install php)");
            ok = false;
        }
    }

    // git
    match Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => {
            success(&format!(
                "git: {}",
                String::from_utf8_lossy(&o.stdout).trim()
            ));
        }
        _ => warning("git: not found (VCS installs will fail)"),
    }

    // cache writable
    let cache = cache_root();
    match std::fs::create_dir_all(&cache) {
        Ok(()) => success(&format!("cache: writable ({})", cache.display())),
        Err(e) => {
            warning(&format!("cache: not writable ({}): {e}", cache.display()));
            ok = false;
        }
    }

    // packagist connectivity
    match reqwest::Client::new()
        .get("https://repo.packagist.org/packages.json")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => success("HTTPS: packagist.org reachable"),
        Ok(r) => {
            warning(&format!("HTTPS: packagist.org returned {}", r.status()));
            ok = false;
        }
        Err(e) => {
            warning(&format!("HTTPS: packagist.org unreachable ({e})"));
            ok = false;
        }
    }

    // auth
    if let Some(p) = global_auth_path() {
        if p.is_file() {
            info(&format!("auth.json (global): {}", p.display()));
        } else {
            info(&format!(
                "auth.json (global): not present ({})",
                p.display()
            ));
        }
    }
    let local = cwd.join("auth.json");
    if local.is_file() {
        info(&format!("auth.json (project): {}", local.display()));
    }
    let _ = AuthStore::load(Some(&cwd));

    if json_path.exists() {
        success(&format!("project: {}", json_path.display()));
    } else {
        info("project: no composer.json in cwd");
    }

    if ok {
        success("diagnose: all critical checks passed");
    } else {
        warning("diagnose: some checks failed — see above");
    }
    Ok(())
}
