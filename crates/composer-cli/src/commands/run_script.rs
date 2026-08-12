//! `composer-rs run-script` / `run`

use super::{info, project_paths, success};
use anyhow::{Result, bail};
use clap::Args;
use composer_manifest::ComposerJson;
use composer_scripts::{list_scripts, run_script};

#[derive(Args, Debug, Clone)]
pub struct RunScriptArgs {
    /// Script name (omit with `--list` to list scripts)
    pub name: Option<String>,

    /// List available scripts
    #[arg(long, short = 'l')]
    pub list: bool,

    /// Extra arguments passed to the script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    #[arg(long)]
    pub no_dev: bool,
}

pub fn run(args: RunScriptArgs) -> Result<()> {
    let (cwd, json_path, _) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    let manifest = ComposerJson::load(&json_path)?;

    if args.list || args.name.is_none() {
        let names = list_scripts(&manifest);
        if names.is_empty() {
            info("No scripts defined in composer.json");
        } else {
            for n in names {
                println!("{n}");
            }
        }
        return Ok(());
    }

    let name = args.name.as_deref().unwrap();
    run_script(&manifest, name, &cwd, &args.args, !args.no_dev)?;
    success(&format!("Script `{name}` finished"));
    Ok(())
}
