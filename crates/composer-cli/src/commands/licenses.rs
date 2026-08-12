//! `composer-rs licenses`

use super::project_paths;
use anyhow::{Result, bail};
use clap::Args;
use composer_lock::{ComposerLock, License};
use std::collections::BTreeMap;

#[derive(Args, Debug, Clone)]
pub struct LicensesArgs {
    #[arg(long, default_value = "text")]
    pub format: String,

    #[arg(long)]
    pub no_dev: bool,
}

pub fn run(args: LicensesArgs) -> Result<()> {
    let (_cwd, _json, lock_path) = project_paths()?;
    if !lock_path.exists() {
        bail!("composer.lock not found");
    }
    let lock = ComposerLock::load(&lock_path)?;
    let with_dev = !args.no_dev;

    let mut by_pkg: Vec<(String, Vec<String>)> = Vec::new();
    let mut summary: BTreeMap<String, usize> = BTreeMap::new();

    for pkg in lock.packages_to_install(with_dev) {
        let licenses: Vec<String> = pkg
            .license
            .iter()
            .map(|l| match l {
                License::One(s) => s.clone(),
                License::Many(v) => v.join(" OR "),
            })
            .collect();
        let display = if licenses.is_empty() {
            vec!["none".into()]
        } else {
            licenses
        };
        for lic in &display {
            *summary.entry(lic.clone()).or_default() += 1;
        }
        by_pkg.push((pkg.name.clone(), display));
    }

    if args.format == "json" {
        let obj = serde_json::json!({
            "packages": by_pkg.iter().map(|(n, l)| serde_json::json!({
                "name": n,
                "license": l,
            })).collect::<Vec<_>>(),
            "summary": summary,
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else if args.format == "summary" {
        for (lic, n) in summary {
            println!("{lic}: {n}");
        }
    } else {
        for (name, lics) in by_pkg {
            println!("{name}: {}", lics.join(", "));
        }
    }
    Ok(())
}
