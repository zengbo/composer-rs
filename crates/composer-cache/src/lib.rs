//! Content-addressable storage (CAS) cache with hardlink install.
//!
//! Design (pnpm / uv inspired):
//! - Packages are stored once under `~/.cache/composer-rs/cas/<shard>/<hash>/`
//! - Project `vendor/` trees hardlink files from the CAS (copy on cross-FS)
//! - Multiple git worktrees share the same physical package bytes

#![deny(unsafe_code)]

use composer_core::error::{Error, Result};
use composer_core::hash::{content_hash, ContentHash};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, warn};
use walkdir::WalkDir;

/// Global cache root (`$COMPOSER_RS_CACHE` or `~/.cache/composer-rs`).
pub fn cache_root() -> PathBuf {
    if let Ok(p) = std::env::var("COMPOSER_RS_CACHE") {
        return PathBuf::from(p);
    }
    directories::BaseDirs::new().map_or_else(
        || PathBuf::from(".composer-rs/cache"),
        |d| d.cache_dir().join("composer-rs"),
    )
}

/// CAS directory for extracted package trees.
pub fn cas_dir() -> PathBuf {
    cache_root().join("cas")
}

/// Directory for downloaded archive blobs.
pub fn archives_dir() -> PathBuf {
    cache_root().join("archives")
}

/// Directory for repository metadata.
pub fn metadata_dir() -> PathBuf {
    cache_root().join("metadata")
}

/// Content-addressable package cache.
#[derive(Debug, Clone)]
pub struct CasCache {
    cas_root: PathBuf,
    /// Per-key mutexes so parallel installs of the same package serialize store().
    store_locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
}

impl Default for CasCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CasCache {
    pub fn new() -> Self {
        Self {
            cas_root: cas_dir(),
            store_locks: Arc::new(DashMap::new()),
        }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            cas_root: root.into(),
            store_locks: Arc::new(DashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.cas_root
    }

    /// Hash a logical package key (dist URL, shasum, etc.) to a CAS id.
    pub fn key_hash(key: &str) -> ContentHash {
        content_hash(key.as_bytes())
    }

    fn entry_path(&self, hash: &ContentHash) -> PathBuf {
        self.cas_root.join(hash.shard()).join(hash.as_str())
    }

    fn marker_path(&self, hash: &ContentHash) -> PathBuf {
        self.entry_path(hash).join(".composer-rs-complete")
    }

    /// Whether package content for `key` is fully present.
    pub fn contains(&self, key: &str) -> bool {
        let hash = Self::key_hash(key);
        self.marker_path(&hash).is_file()
    }

    /// Path to cached package tree if complete.
    pub fn get(&self, key: &str) -> Option<PathBuf> {
        let hash = Self::key_hash(key);
        let path = self.entry_path(&hash);
        if self.marker_path(&hash).is_file() {
            Some(path)
        } else {
            None
        }
    }

    /// Store extracted package directory under CAS key.
    ///
    /// Atomic: extract to temp, then rename into place and write marker.
    pub fn store(&self, key: &str, source_dir: &Path) -> Result<PathBuf> {
        let hash = Self::key_hash(key);
        let dest = self.entry_path(&hash);

        if self.marker_path(&hash).is_file() {
            return Ok(dest);
        }

        let lock = self
            .store_locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock();

        if self.marker_path(&hash).is_file() {
            return Ok(dest);
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        // Staging directory next to final destination
        let staging = dest.with_extension("staging");
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;
        }
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|e| Error::io(&dest, e))?;
        }

        copy_dir_recursive(source_dir, &staging)?;
        fs::rename(&staging, &dest).map_err(|e| Error::io(&dest, e))?;
        fs::write(self.marker_path(&hash), key.as_bytes())
            .map_err(|e| Error::io(self.marker_path(&hash), e))?;

        debug!(key = %key, path = %dest.display(), "stored package in CAS");
        Ok(dest)
    }

    /// Link (hardlink) or copy cached package into project vendor path.
    pub fn link_to(&self, key: &str, dest: &Path) -> Result<LinkResult> {
        let cache_path = self
            .get(key)
            .ok_or_else(|| Error::Cache(format!("cache miss for key: {key}")))?;

        if dest.exists() {
            fs::remove_dir_all(dest).map_err(|e| Error::io(dest, e))?;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let stats = link_dir_recursive(&cache_path, dest)?;
        Ok(stats)
    }

    /// Ensure dest matches cache; on miss call `populate` then store + link.
    pub fn install_or_populate<F>(&self, key: &str, dest: &Path, populate: F) -> Result<InstallKind>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        if self.contains(key) {
            let stats = self.link_to(key, dest)?;
            return Ok(InstallKind::CacheHit {
                hardlinks: stats.hardlinks,
                copies: stats.copies,
            });
        }

        let tmp = tempfile::tempdir().map_err(|e| Error::Cache(e.to_string()))?;
        populate(tmp.path())?;
        self.store(key, tmp.path())?;
        let stats = self.link_to(key, dest)?;
        Ok(InstallKind::CacheMiss {
            hardlinks: stats.hardlinks,
            copies: stats.copies,
        })
    }

    /// Total bytes used by CAS.
    pub fn size_bytes(&self) -> u64 {
        dir_size(&self.cas_root)
    }

    /// Number of complete cached packages.
    pub fn package_count(&self) -> Result<usize> {
        if !self.cas_root.exists() {
            return Ok(0);
        }
        let mut count = 0;
        for shard in fs::read_dir(&self.cas_root).map_err(|e| Error::io(&self.cas_root, e))? {
            let shard = shard.map_err(|e| Error::io(&self.cas_root, e))?;
            if !shard.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            for entry in fs::read_dir(shard.path()).map_err(|e| Error::io(shard.path(), e))? {
                let entry = entry.map_err(|e| Error::io(shard.path(), e))?;
                if entry.path().join(".composer-rs-complete").is_file() {
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// Remove entire cache root and return bytes freed.
    pub fn clear_all() -> Result<u64> {
        let root = cache_root();
        if !root.exists() {
            return Ok(0);
        }
        let size = dir_size(&root);
        fs::remove_dir_all(&root).map_err(|e| Error::io(&root, e))?;
        Ok(size)
    }
}

/// How a package was installed into vendor.
#[derive(Debug, Clone)]
pub enum InstallKind {
    CacheHit { hardlinks: u64, copies: u64 },
    CacheMiss { hardlinks: u64, copies: u64 },
}

impl InstallKind {
    pub fn is_hit(&self) -> bool {
        matches!(self, Self::CacheHit { .. })
    }
}

#[derive(Debug, Clone, Default)]
pub struct LinkResult {
    pub hardlinks: u64,
    pub copies: u64,
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).map_err(|e| Error::io(dst, e))?;
    for entry in fs::read_dir(src).map_err(|e| Error::io(src, e))? {
        let entry = entry.map_err(|e| Error::io(src, e))?;
        let name = entry.file_name();
        if name == ".composer-rs-complete" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        let ft = entry.file_type().map_err(|e| Error::io(&src_path, e))?;
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if ft.is_file() {
            fs::copy(&src_path, &dst_path).map_err(|e| Error::io(&dst_path, e))?;
        } else if ft.is_symlink() {
            // Preserve symlink target when possible
            #[cfg(unix)]
            {
                let target = fs::read_link(&src_path).map_err(|e| Error::io(&src_path, e))?;
                std::os::unix::fs::symlink(&target, &dst_path)
                    .map_err(|e| Error::io(&dst_path, e))?;
            }
            #[cfg(not(unix))]
            {
                fs::copy(&src_path, &dst_path).map_err(|e| Error::io(&dst_path, e))?;
            }
        }
    }
    Ok(())
}

fn link_dir_recursive(src: &Path, dst: &Path) -> Result<LinkResult> {
    let mut stats = LinkResult::default();
    fs::create_dir_all(dst).map_err(|e| Error::io(dst, e))?;

    for entry in fs::read_dir(src).map_err(|e| Error::io(src, e))? {
        let entry = entry.map_err(|e| Error::io(src, e))?;
        let name = entry.file_name();
        if name == ".composer-rs-complete" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        let ft = entry.file_type().map_err(|e| Error::io(&src_path, e))?;

        if ft.is_dir() {
            let child = link_dir_recursive(&src_path, &dst_path)?;
            stats.hardlinks += child.hardlinks;
            stats.copies += child.copies;
        } else if ft.is_file() {
            match fs::hard_link(&src_path, &dst_path) {
                Ok(()) => stats.hardlinks += 1,
                Err(e) => {
                    // Cross-device or unsupported → copy
                    debug!(
                        src = %src_path.display(),
                        dst = %dst_path.display(),
                        error = %e,
                        "hardlink failed, copying"
                    );
                    fs::copy(&src_path, &dst_path).map_err(|e| Error::io(&dst_path, e))?;
                    stats.copies += 1;
                }
            }
        } else if ft.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&src_path).map_err(|e| Error::io(&src_path, e))?;
                if let Err(e) = std::os::unix::fs::symlink(&target, &dst_path) {
                    warn!("symlink recreate failed: {e}");
                    fs::copy(&src_path, &dst_path).map_err(|e| Error::io(&dst_path, e))?;
                    stats.copies += 1;
                } else {
                    stats.hardlinks += 1;
                }
            }
            #[cfg(not(unix))]
            {
                fs::copy(&src_path, &dst_path).map_err(|e| Error::io(&dst_path, e))?;
                stats.copies += 1;
            }
        }
    }
    Ok(stats)
}

fn dir_size(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Format byte count for display.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn store_and_link_hardlink() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = CasCache::with_root(tmp.path().join("cas"));

        let pkg = tmp.path().join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        let mut f = fs::File::create(pkg.join("hello.txt")).unwrap();
        writeln!(f, "world").unwrap();

        cas.store("key-1", &pkg).unwrap();
        assert!(cas.contains("key-1"));

        let vendor = tmp.path().join("vendor/foo/bar");
        let kind = cas.link_to("key-1", &vendor).unwrap();
        assert!(vendor.join("hello.txt").is_file());
        assert!(kind.hardlinks + kind.copies >= 1);

        // Second project path shares inode on Unix when hardlinked
        let vendor2 = tmp.path().join("vendor2/foo/bar");
        cas.link_to("key-1", &vendor2).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ino1 = fs::metadata(vendor.join("hello.txt")).unwrap().ino();
            let ino2 = fs::metadata(vendor2.join("hello.txt")).unwrap().ino();
            let cas_ino = fs::metadata(
                cas.get("key-1").unwrap().join("hello.txt"),
            )
            .unwrap()
            .ino();
            assert_eq!(ino1, cas_ino);
            assert_eq!(ino2, cas_ino);
        }
    }
}
