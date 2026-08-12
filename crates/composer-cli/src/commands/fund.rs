//! `composer-rs fund` — list funding links from installed packages.

use super::{info, project_paths, vendor_dir};
use anyhow::{Result, bail};
use clap::Args;
use composer_lock::ComposerLock;
use composer_manifest::ComposerJson;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;

#[derive(Args, Debug, Clone)]
pub struct FundArgs {
    #[arg(long, default_value = "text")]
    pub format: String,

    #[arg(long)]
    pub no_dev: bool,
}

#[derive(Debug, serde::Serialize)]
struct FundEntry {
    package: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fund_type: Option<String>,
}

pub fn run(args: FundArgs) -> Result<()> {
    let (cwd, json_path, lock_path) = project_paths()?;
    if !lock_path.exists() {
        bail!("composer.lock not found");
    }
    let manifest = ComposerJson::load(&json_path)?;
    let lock = ComposerLock::load(&lock_path)?;
    let vendor = vendor_dir(&manifest, &cwd);
    let paths = manifest.installer_paths();
    let with_dev = !args.no_dev;

    let mut entries = Vec::new();
    for pkg in lock.packages_to_install(with_dev) {
        let dest = paths
            .resolve(&cwd, &pkg.name, pkg.package_type.as_deref())
            .unwrap_or_else(|| vendor.join(&pkg.name));
        let cj = dest.join("composer.json");
        if !cj.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&cj) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(funding) = doc.get("funding") else {
            continue;
        };
        collect_funding(&pkg.name, funding, &mut entries);
    }

    if args.format == "json" {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() {
        info("No funding links found in installed packages");
    } else {
        let mut by_pkg: BTreeMap<String, Vec<&FundEntry>> = BTreeMap::new();
        for e in &entries {
            by_pkg.entry(e.package.clone()).or_default().push(e);
        }
        for (pkg, list) in by_pkg {
            println!("{pkg}");
            for e in list {
                if let Some(t) = &e.fund_type {
                    println!("  [{t}] {}", e.url);
                } else {
                    println!("  {}", e.url);
                }
            }
        }
    }
    Ok(())
}

fn collect_funding(package: &str, funding: &Value, out: &mut Vec<FundEntry>) {
    match funding {
        Value::Array(arr) => {
            for item in arr {
                if let Some(url) = item.get("url").and_then(|u| u.as_str()) {
                    out.push(FundEntry {
                        package: package.into(),
                        url: url.into(),
                        fund_type: item
                            .get("type")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string()),
                    });
                } else if let Some(url) = item.as_str() {
                    out.push(FundEntry {
                        package: package.into(),
                        url: url.into(),
                        fund_type: None,
                    });
                }
            }
        }
        Value::Object(map) => {
            if let Some(url) = map.get("url").and_then(|u| u.as_str()) {
                out.push(FundEntry {
                    package: package.into(),
                    url: url.into(),
                    fund_type: map
                        .get("type")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string()),
                });
            }
        }
        Value::String(url) => out.push(FundEntry {
            package: package.into(),
            url: url.clone(),
            fund_type: None,
        }),
        _ => {}
    }
}
