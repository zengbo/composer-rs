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
    /// Delete CAS packages not hardlinked from any vendor
    #[command(visible_alias = "gc")]
    Prune {
        /// List unreferenced packages without deleting
        #[arg(long)]
        dry_run: bool,
    },
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
            let estimate = cas.prune_unreferenced(true)?;
            info(&format!(
                "unref.   : {} package(s) · {} (estimate)",
                estimate.removed(),
                format_bytes(estimate.bytes_freed)
            ));
            #[cfg(not(unix))]
            super::warning("complete packages are not pruned on this platform");
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
        CacheCommands::Prune { dry_run } => {
            header(if dry_run {
                "Pruning unreferenced CAS (dry run)"
            } else {
                "Pruning unreferenced CAS"
            });
            let cas = CasCache::new();
            let stats = cas.prune_unreferenced(dry_run)?;
            let removed_label = if dry_run { "would remove" } else { "removed" };
            info(&format!(
                "complete : {} scanned  ·  {} in use  ·  {} {removed_label}",
                stats.complete_scanned, stats.complete_kept, stats.complete_removed
            ));
            if stats.leftover_removed > 0 {
                info(&format!(
                    "leftover : {} incomplete/staging",
                    stats.leftover_removed
                ));
            }
            info("archives : left in place");
            #[cfg(not(unix))]
            super::warning("complete packages are not pruned on this platform");
            if dry_run {
                success(&format!(
                    "Dry run — would free {} ({} package tree(s))",
                    format_bytes(stats.bytes_freed),
                    stats.removed()
                ));
            } else {
                success(&format!(
                    "Pruned {} ({} package tree(s))",
                    format_bytes(stats.bytes_freed),
                    stats.removed()
                ));
            }
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
