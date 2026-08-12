//! `composer-rs search`

use super::{header, project_paths, success};
use anyhow::{Result, bail};
use clap::Args;
use composer_auth::AuthStore;
use composer_manifest::ComposerJson;
use composer_repo::RepositoryRegistry;

#[derive(Args, Debug, Clone)]
pub struct SearchArgs {
    /// Search terms
    pub query: Vec<String>,

    #[arg(long, default_value_t = 15)]
    pub limit: usize,
}

pub async fn run(args: SearchArgs) -> Result<()> {
    if args.query.is_empty() {
        bail!("provide a search term");
    }
    let q = args.query.join(" ");
    header(&format!("Search: {q}"));

    let (cwd, json_path, _) = project_paths()?;
    let auth = AuthStore::load(Some(&cwd)).unwrap_or_default();

    let results = if json_path.exists() {
        let manifest = ComposerJson::load(&json_path)?;
        let registry = RepositoryRegistry::from_manifest_auth(&manifest, auth)?;
        registry.search(&q, args.limit).await?
    } else {
        // No project: search public Packagist with global/env auth if any.
        let client = composer_repo::RepositoryClient::with_base_url_auth(
            "https://repo.packagist.org",
            auth,
        )?;
        client.search(&q, args.limit).await?
    };

    if results.is_empty() {
        println!("No results.");
        return Ok(());
    }

    for r in &results {
        let desc = r.description.as_deref().unwrap_or("");
        let dl = r.downloads.unwrap_or(0);
        println!("{:<32}  {:>10} downloads  {}", r.name, dl, desc);
    }
    success(&format!("{} result(s)", results.len()));
    Ok(())
}
