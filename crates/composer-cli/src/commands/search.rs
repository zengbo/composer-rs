//! `composer-rs search`

use super::{header, success};
use anyhow::{bail, Result};
use clap::Args;
use composer_repo::RepositoryClient;

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

    let client = RepositoryClient::new()?;
    let results = client.search(&q, args.limit).await?;
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
