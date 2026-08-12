//! `composer-rs init`

use super::{header, project_paths, success};
use anyhow::{Result, bail};
use clap::Args;
use composer_manifest::ComposerJson;
use std::collections::BTreeMap;

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// Package name (vendor/name)
    #[arg(long)]
    pub name: Option<String>,

    /// Description
    #[arg(long)]
    pub description: Option<String>,

    /// Stability
    #[arg(long, default_value = "stable")]
    pub stability: String,

    /// Overwrite existing composer.json
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: InitArgs) -> Result<()> {
    header("Initialize composer.json");
    let (cwd, json_path, _) = project_paths()?;
    if json_path.exists() && !args.force {
        bail!(
            "{} already exists (use --force to overwrite)",
            json_path.display()
        );
    }

    let name = args.name.unwrap_or_else(|| {
        format!(
            "vendor/{}",
            cwd.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("project")
        )
    });

    let mut require = BTreeMap::new();
    require.insert("php".into(), ">=8.1".into());

    let manifest = ComposerJson {
        name: Some(name),
        description: args.description.or_else(|| Some("".into())),
        package_type: Some("project".into()),
        require,
        minimum_stability: Some(args.stability),
        prefer_stable: Some(true),
        ..Default::default()
    };

    manifest.save(&json_path)?;
    success(&format!("Wrote {}", json_path.display()));
    Ok(())
}
