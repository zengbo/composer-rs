//! `composer-rs cache`

use super::{header, info, success};
use anyhow::Result;
use clap::Subcommand;
use composer_cache::{CasCache, archives_dir, format_bytes, metadata_dir};
use std::fs;

#[derive(Subcommand, Debug, Clone)]
pub enum CacheCommands {
    /// Clear the entire content-addressable cache
    Clear,
    /// Show cache location and size
    Info,
    /// Print absolute cache root directory
    Dir,
    /// Show repository metadata cache path / size
    Repo,
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
            info(&format!("archives : {}", archives_dir().display()));
            info(&format!("metadata : {}", metadata_dir().display()));
            info(&format!("packages : {}", cas.package_count()?));
            info(&format!("size     : {}", format_bytes(cas.size_bytes())));
            success("done");
        }
        CacheCommands::Dir => {
            println!("{}", composer_cache::cache_root().display());
        }
        CacheCommands::Repo => {
            header("Repository metadata cache");
            let meta = metadata_dir();
            info(&format!("path : {}", meta.display()));
            let size = dir_size(&meta);
            info(&format!("size : {}", format_bytes(size)));
            if meta.is_dir() {
                let count = count_files(&meta);
                info(&format!("files: {count}"));
            } else {
                info("files: 0 (not created yet)");
            }
            success("done");
        }
    }
    Ok(())
}

fn dir_size(path: &std::path::Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn count_files(path: &std::path::Path) -> usize {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count()
}

#[allow(dead_code)]
fn ensure_dir(path: &std::path::Path) {
    let _ = fs::create_dir_all(path);
}
