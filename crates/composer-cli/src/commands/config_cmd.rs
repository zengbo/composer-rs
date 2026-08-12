//! `composer-rs config`

use super::{info, project_paths, success};
use anyhow::{Result, bail};
use clap::Args;
use composer_auth::{AuthStore, global_auth_path};
use composer_manifest::ComposerJson;
use serde_json::Value;

#[derive(Args, Debug, Clone)]
pub struct ConfigArgs {
    /// List all config values
    #[arg(long, short = 'l')]
    pub list: bool,

    /// Operate on global config (auth path info)
    #[arg(long, short = 'g')]
    pub global: bool,

    /// Show auth.json paths / summary (read-only)
    #[arg(long)]
    pub auth: bool,

    /// Unset a key
    #[arg(long)]
    pub unset: bool,

    /// Config key (dotted, e.g. platform.php, vendor-dir)
    pub key: Option<String>,

    /// Value to set
    pub value: Option<String>,
}

pub fn run(args: ConfigArgs) -> Result<()> {
    let (cwd, json_path, _) = project_paths()?;

    if args.auth {
        if let Some(p) = global_auth_path() {
            println!("global-auth: {}", p.display());
        }
        let local = cwd.join("auth.json");
        println!("local-auth:  {}", local.display());
        let store = AuthStore::load(Some(&cwd)).unwrap_or_default();
        println!(
            "http-basic hosts: {}",
            store
                .http_basic
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    }

    if args.global {
        info("Global composer.json editing is limited; use --auth for auth.json location");
        if let Some(p) = global_auth_path() {
            println!("{}", p.display());
        }
        return Ok(());
    }

    if !json_path.exists() {
        bail!("composer.json not found");
    }
    let mut manifest = ComposerJson::load(&json_path)?;

    if args.list {
        if let Some(cfg) = &manifest.config {
            println!("{}", serde_json::to_string_pretty(cfg)?);
        } else {
            info("No config section in composer.json");
        }
        println!("vendor-dir = {}", manifest.vendor_dir());
        println!("bin-dir = {}", manifest.bin_dir());
        return Ok(());
    }

    let Some(key) = args.key.as_deref() else {
        bail!("provide a config key or --list");
    };

    if args.unset {
        // shallow unset for top-level config keys
        if let Some(Value::Object(map)) = manifest.config.as_mut() {
            map.remove(key);
            if let Some((head, _)) = key.split_once('.') {
                // only full key path simple remove for nested via rebuild
                let _ = head;
            }
        }
        // Nested: set null by rebuilding without key is complex; support simple keys
        if key.contains('.') {
            // set to null effectively by removing leaf
            if let Some(Value::Object(root)) = manifest.config.as_mut() {
                remove_dotted(root, key);
            }
        }
        manifest.save(&json_path)?;
        success(&format!("Unset config.{key}"));
        return Ok(());
    }

    if let Some(val) = &args.value {
        let json_val = serde_json::from_str(val).unwrap_or(Value::String(val.clone()));
        match key {
            "vendor-dir" | "bin-dir" => {
                manifest.config_set(key, json_val);
            }
            k if k.starts_with("platform.")
                || k == "platform"
                || k.starts_with("allow-plugins") =>
            {
                manifest.config_set(k, json_val);
            }
            _ => {
                manifest.config_set(key, json_val);
            }
        }
        manifest.save(&json_path)?;
        success(&format!("Set config.{key}"));
        return Ok(());
    }

    // get
    match key {
        "vendor-dir" => println!("{}", manifest.vendor_dir()),
        "bin-dir" => println!("{}", manifest.bin_dir()),
        _ => {
            if let Some(v) = manifest.config_get(key) {
                match v {
                    Value::String(s) => println!("{s}"),
                    other => println!("{other}"),
                }
            } else {
                bail!("config key `{key}` is not set");
            }
        }
    }
    Ok(())
}

fn remove_dotted(map: &mut serde_json::Map<String, Value>, key: &str) {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.len() == 1 {
        map.remove(parts[0]);
        return;
    }
    if let Some(Value::Object(child)) = map.get_mut(parts[0]) {
        remove_dotted(child, &parts[1..].join("."));
    }
}
