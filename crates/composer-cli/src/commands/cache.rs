//! `composer-rs cache`

use super::{header, info, success};
use anyhow::Result;
use clap::Subcommand;
use composer_cache::{format_bytes, CasCache};

#[derive(Subcommand, Debug, Clone)]
pub enum CacheCommands {
    /// Clear the entire content-addressable cache
    Clear,
    /// Show cache location and size
    Info,
}

pub fn run(cmd: CacheCommands) -> Result<()> {
    match cmd {
        CacheCommands::Clear => {
            header("Clearing cache");
            let freed = CasCache::clear_all()?;
            success(&format!("Cleared {} of cache", format_bytes(freed)));
        }
        CacheCommands::Info => {
            header("Cache info");
            let cas = CasCache::new();
            let root = composer_cache::cache_root();
            info(&format!("root     : {}", root.display()));
            info(&format!("CAS      : {}", cas.root().display()));
            info(&format!("packages : {}", cas.package_count()?));
            info(&format!("size     : {}", format_bytes(cas.size_bytes())));
            success("done");
        }
    }
    Ok(())
}
