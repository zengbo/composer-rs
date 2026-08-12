//! `composer-rs require`

use super::{header, info, project_paths, success};
use anyhow::{bail, Context, Result};
use clap::Args;
use composer_core::PackageId;
use composer_manifest::ComposerJson;
use composer_repo::RepositoryRegistry;

#[derive(Args, Debug, Clone)]
pub struct RequireArgs {
    /// Package name, optionally with version: vendor/package:^1.0
    pub packages: Vec<String>,

    /// Add to require-dev
    #[arg(long)]
    pub dev: bool,

    /// Do not run update after modifying composer.json
    #[arg(long)]
    pub no_update: bool,

    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: RequireArgs) -> Result<()> {
    header("Require package");
    if args.packages.is_empty() {
        bail!("specify at least one package (vendor/name or vendor/name:constraint)");
    }

    let (cwd, json_path, _) = project_paths()?;
    let mut manifest = if json_path.exists() {
        ComposerJson::load(&json_path)?
    } else {
        ComposerJson {
            name: Some("vendor/project".into()),
            ..Default::default()
        }
    };

    let client = RepositoryRegistry::from_manifest(&manifest)?;

    for spec in &args.packages {
        let (name, constraint) = parse_package_spec(spec);
        let id = PackageId::parse(&name).context("package name")?;

        let constraint = if constraint == "*" {
            // Pick a sensible caret from latest stable
            match client
                .find_best(&id, &composer_core::VersionConstraint::any(), true, "stable")
                .await
            {
                Ok(best) => {
                    let v = &best.version;
                    let (maj, min, _) = parse_parts(v.normalized());
                    if maj > 0 {
                        format!("^{maj}.0")
                    } else {
                        format!("^{maj}.{min}")
                    }
                }
                Err(_) => "*".into(),
            }
        } else {
            constraint
        };

        info(&format!("Using version constraint {constraint} for {name}"));

        if args.dev {
            manifest.require_dev.insert(name.clone(), constraint);
            manifest.require.remove(&name);
        } else {
            manifest.require.insert(name.clone(), constraint);
            manifest.require_dev.remove(&name);
        }
    }

    if args.dry_run {
        success("Dry run — composer.json not modified");
        return Ok(());
    }

    manifest.save(&json_path)?;
    success(&format!("Updated {}", json_path.display()));

    if !args.no_update {
        let update_args = super::update::UpdateArgs {
            packages: vec![],
            no_dev: false,
            prefer_lowest: false,
            prefer_stable: true,
            dry_run: false,
            optimize_autoloader: false,
            classmap_authoritative: false,
            concurrency: None,
            verify_checksums: false,
            ignore_platform_reqs: false,
            prefer_dist: true,
            prefer_source: false,
        };
        super::update::run(update_args).await?;
    }

    let _ = cwd;
    Ok(())
}

fn parse_package_spec(spec: &str) -> (String, String) {
    // vendor/name:^1.0 or vendor/name=^1.0 or vendor/name ^1.0
    if let Some((n, c)) = spec.split_once(':') {
        return (n.to_string(), c.to_string());
    }
    if let Some((n, c)) = spec.split_once('=') {
        return (n.to_string(), c.to_string());
    }
    let parts: Vec<_> = spec.split_whitespace().collect();
    if parts.len() >= 2 {
        return (parts[0].to_string(), parts[1..].join(" "));
    }
    (spec.to_string(), "*".into())
}

fn parse_parts(normalized: &str) -> (u64, u64, u64) {
    let mut it = normalized.split('.');
    let maj = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let min = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let pat = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (maj, min, pat)
}
