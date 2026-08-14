//! Content-addressable storage (CAS) cache with hardlink install.
//!
//! Design (pnpm / uv inspired):
//! - Packages are stored once under `~/.cache/composer-rs/cas/<shard>/<hash>/`
//! - Project `vendor/` trees hardlink files from the CAS (copy on cross-FS)
//! - Multiple git worktrees share the same physical package bytes
//! - `prune_unreferenced` drops CAS trees whose files have no vendor hardlinks

#![deny(unsafe_code)]

use composer_core::error::{Error, Result};
use composer_core::hash::{ContentHash, content_hash};
use dashmap::DashMap;
use fs4::fs_std::FileExt;
use parking_lot::Mutex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
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

        let lock_path = dest.with_extension("lock");
        let _file_lock = acquire_exclusive_lock(&lock_path)?;

        if self.marker_path(&hash).is_file() {
            return Ok(dest);
        }

        // Unique staging dir so concurrent processes cannot delete each other.
        let staging = dest.with_file_name(format!(
            "{}.staging-{}-{}",
            dest.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            unique_suffix()
        ));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;
        }

        copy_dir_recursive(source_dir, &staging)?;
        make_tree_readonly(&staging)?;

        if dest.exists() && !self.marker_path(&hash).is_file() {
            fs::remove_dir_all(&dest).map_err(|e| Error::io(&dest, e))?;
        }
        if dest.exists() && self.marker_path(&hash).is_file() {
            let _ = fs::remove_dir_all(&staging);
            return Ok(dest);
        }
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

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let tmp = dest.with_file_name(format!(
            "{}.composer-rs-link-{}-{}",
            dest.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            unique_suffix()
        ));
        if tmp.exists() {
            fs::remove_dir_all(&tmp).map_err(|e| Error::io(&tmp, e))?;
        }

        let stats = match link_dir_recursive(&cache_path, &tmp) {
            Ok(s) => s,
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                return Err(e);
            }
        };

        if dest.exists() {
            let bak = dest.with_file_name(format!(
                "{}.composer-rs-old-{}-{}",
                dest.file_name().unwrap_or_default().to_string_lossy(),
                std::process::id(),
                unique_suffix()
            ));
            fs::rename(dest, &bak).map_err(|e| Error::io(dest, e))?;
            if let Err(e) = fs::rename(&tmp, dest) {
                let _ = fs::rename(&bak, dest);
                let _ = fs::remove_dir_all(&tmp);
                return Err(Error::io(dest, e));
            }
            let _ = fs::remove_dir_all(&bak);
        } else if let Err(e) = fs::rename(&tmp, dest) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(Error::io(dest, e));
        }
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

    /// Drop CAS trees that no vendor hardlinks to.
    ///
    /// A complete package is live if any regular file (other than the
    /// `.composer-rs-complete` marker) has `nlink > 1`. That is the install
    /// path: `link_to` hardlinks vendor files onto the CAS inodes.
    ///
    /// Incomplete / `.staging` leftovers are always candidates. Archives and
    /// metadata are left alone. On non-Unix, hardlink nlink is not available,
    /// so complete packages are kept.
    ///
    /// If vendor was populated with `copy` (hardlink failed, usually
    /// cross-filesystem), CAS files stay at `nlink == 1` and this will treat
    /// them as orphans. The vendor copies remain valid.
    ///
    /// `dry_run` reports the same counts without deleting.
    pub fn prune_unreferenced(&self, dry_run: bool) -> Result<PruneStats> {
        let mut stats = PruneStats::default();
        if !self.cas_root.exists() {
            return Ok(stats);
        }

        let shards: Vec<PathBuf> = fs::read_dir(&self.cas_root)
            .map_err(|e| Error::io(&self.cas_root, e))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| Error::io(&self.cas_root, e))?;

        for shard_path in shards {
            if !shard_path.is_dir() {
                continue;
            }
            self.prune_shard(&shard_path, dry_run, &mut stats)?;
            if !dry_run {
                remove_dir_if_empty(&shard_path);
            }
        }
        Ok(stats)
    }

    fn prune_shard(&self, shard_path: &Path, dry_run: bool, stats: &mut PruneStats) -> Result<()> {
        let entries: Vec<PathBuf> = fs::read_dir(shard_path)
            .map_err(|e| Error::io(shard_path, e))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| Error::io(shard_path, e))?;

        for path in entries {
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let complete = path.join(".composer-rs-complete").is_file();
            if name.contains(".staging") || !complete {
                stats.leftover_removed += 1;
                drop_cas_tree(&path, dry_run, stats)?;
                continue;
            }

            stats.complete_scanned += 1;
            if package_has_vendor_hardlink(&path) {
                stats.complete_kept += 1;
                continue;
            }
            if !remove_unreferenced_complete(&path, dry_run, stats)? {
                stats.complete_kept += 1;
            }
        }
        Ok(())
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

/// Result of [`CasCache::prune_unreferenced`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneStats {
    /// Complete CAS packages examined.
    pub complete_scanned: usize,
    /// Complete packages still hardlinked from a vendor.
    pub complete_kept: usize,
    /// Complete packages deleted (or counted, when dry-run).
    pub complete_removed: usize,
    /// Incomplete / staging trees deleted (or counted, when dry-run).
    pub leftover_removed: usize,
    /// Bytes that were (or would be) freed.
    pub bytes_freed: u64,
}

impl PruneStats {
    pub fn removed(&self) -> usize {
        self.complete_removed + self.leftover_removed
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

/// True when any CAS file inode is also linked from outside this tree.
///
/// Unreadable entries are treated as live (do not GC). On non-Unix we cannot
/// read `nlink`, so complete packages are treated as live.
fn package_has_vendor_hardlink(path: &Path) -> bool {
    #[cfg(not(unix))]
    {
        let _ = path;
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        for entry in WalkDir::new(path) {
            let Ok(entry) = entry else {
                return true;
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.file_name() == ".composer-rs-complete" {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                return true;
            };
            if meta.nlink() > 1 {
                return true;
            }
        }
        false
    }
}

fn drop_cas_tree(path: &Path, dry_run: bool, stats: &mut PruneStats) -> Result<()> {
    let size = dir_size(path);
    if !dry_run {
        fs::remove_dir_all(path).map_err(|e| Error::io(path, e))?;
    }
    stats.bytes_freed += size;
    Ok(())
}

/// Delete an unreferenced complete package. Returns `false` if a vendor
/// hardlink appeared after the marker was dropped (package is kept).
fn remove_unreferenced_complete(
    path: &Path,
    dry_run: bool,
    stats: &mut PruneStats,
) -> Result<bool> {
    if dry_run {
        stats.complete_removed += 1;
        stats.bytes_freed += dir_size(path);
        return Ok(true);
    }

    let marker = path.join(".composer-rs-complete");
    let key = fs::read(&marker).map_err(|e| Error::io(&marker, e))?;
    fs::remove_file(&marker).map_err(|e| Error::io(&marker, e))?;

    if package_has_vendor_hardlink(path) {
        fs::write(&marker, key).map_err(|e| Error::io(&marker, e))?;
        return Ok(false);
    }

    stats.complete_removed += 1;
    drop_cas_tree(path, false, stats)?;
    Ok(true)
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn acquire_exclusive_lock(path: &Path) -> Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| Error::io(path, e))?;
    file.lock_exclusive()
        .map_err(|e| Error::Cache(format!("lock {}: {e}", path.display())))?;
    Ok(file)
}

fn make_tree_readonly(path: &Path) -> Result<()> {
    for entry in WalkDir::new(path) {
        let entry = entry.map_err(|e| Error::Cache(e.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name() == ".composer-rs-complete" {
            continue;
        }
        let meta = fs::metadata(entry.path()).map_err(|e| Error::io(entry.path(), e))?;
        let mut perms = meta.permissions();
        if !perms.readonly() {
            perms.set_readonly(true);
            fs::set_permissions(entry.path(), perms).map_err(|e| Error::io(entry.path(), e))?;
        }
    }
    Ok(())
}

fn remove_dir_if_empty(path: &Path) {
    if let Ok(mut entries) = fs::read_dir(path) {
        if entries.next().is_none() {
            let _ = fs::remove_dir(path);
        }
    }
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
            let cas_ino = fs::metadata(cas.get("key-1").unwrap().join("hello.txt"))
                .unwrap()
                .ino();
            assert_eq!(ino1, cas_ino);
            assert_eq!(ino2, cas_ino);
        }

        let dest_file = vendor.join("hello.txt");
        assert!(
            fs::metadata(&dest_file).unwrap().permissions().readonly(),
            "CAS-linked vendor files must be read-only"
        );
        assert!(fs::write(&dest_file, "mutated").is_err());
        let cas_body = fs::read_to_string(cas.get("key-1").unwrap().join("hello.txt")).unwrap();
        assert_eq!(cas_body, "world\n");
    }

    #[test]
    fn vendor_write_cannot_mutate_cas() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = CasCache::with_root(tmp.path().join("cas"));
        let pkg = tmp.path().join("pkg");
        write_pkg(&pkg, "original");
        cas.store("immut", &pkg).unwrap();
        let a = tmp.path().join("a/vendor/pkg");
        let b = tmp.path().join("b/vendor/pkg");
        cas.link_to("immut", &a).unwrap();
        cas.link_to("immut", &b).unwrap();
        assert!(fs::write(a.join("hello.txt"), "pwned").is_err());
        assert_eq!(
            fs::read_to_string(b.join("hello.txt")).unwrap(),
            "original\n"
        );
        assert_eq!(
            fs::read_to_string(cas.get("immut").unwrap().join("hello.txt")).unwrap(),
            "original\n"
        );
    }

    fn write_pkg(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        let mut f = fs::File::create(dir.join("hello.txt")).unwrap();
        writeln!(f, "{body}").unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn prune_drops_unlinked_package() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = CasCache::with_root(tmp.path().join("cas"));
        let pkg = tmp.path().join("pkg");
        write_pkg(&pkg, "orphan");
        cas.store("orphan-key", &pkg).unwrap();
        assert_eq!(cas.package_count().unwrap(), 1);

        let dry = cas.prune_unreferenced(true).unwrap();
        assert_eq!(dry.complete_scanned, 1);
        assert_eq!(dry.complete_removed, 1);
        assert_eq!(dry.complete_kept, 0);
        assert!(dry.bytes_freed > 0);
        assert_eq!(cas.package_count().unwrap(), 1);

        let gone = cas.prune_unreferenced(false).unwrap();
        assert_eq!(gone.complete_removed, 1);
        assert_eq!(cas.package_count().unwrap(), 0);
        assert!(!cas.contains("orphan-key"));
    }

    #[test]
    fn prune_keeps_hardlinked_vendor() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = CasCache::with_root(tmp.path().join("cas"));
        let pkg = tmp.path().join("pkg");
        write_pkg(&pkg, "live");
        cas.store("live-key", &pkg).unwrap();
        let vendor = tmp.path().join("vendor/foo/bar");
        cas.link_to("live-key", &vendor).unwrap();

        let stats = cas.prune_unreferenced(false).unwrap();
        assert_eq!(stats.complete_scanned, 1);
        assert_eq!(stats.complete_kept, 1);
        assert_eq!(stats.complete_removed, 0);
        assert!(cas.contains("live-key"));
        assert!(vendor.join("hello.txt").is_file());
    }

    #[test]
    fn prune_keeps_if_any_vendor_remains() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = CasCache::with_root(tmp.path().join("cas"));
        let pkg = tmp.path().join("pkg");
        write_pkg(&pkg, "shared");
        cas.store("shared-key", &pkg).unwrap();
        let vendor_a = tmp.path().join("a/vendor/foo/bar");
        let vendor_b = tmp.path().join("b/vendor/foo/bar");
        cas.link_to("shared-key", &vendor_a).unwrap();
        cas.link_to("shared-key", &vendor_b).unwrap();
        fs::remove_dir_all(&vendor_a).unwrap();

        let stats = cas.prune_unreferenced(false).unwrap();
        assert_eq!(stats.complete_kept, 1);
        assert_eq!(stats.complete_removed, 0);
        assert!(cas.contains("shared-key"));
        assert!(vendor_b.join("hello.txt").is_file());
    }

    #[test]
    #[cfg(unix)]
    fn prune_drops_after_last_vendor_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = CasCache::with_root(tmp.path().join("cas"));
        let pkg = tmp.path().join("pkg");
        write_pkg(&pkg, "gone");
        cas.store("gone-key", &pkg).unwrap();
        let vendor = tmp.path().join("vendor/foo/bar");
        cas.link_to("gone-key", &vendor).unwrap();
        fs::remove_dir_all(&vendor).unwrap();

        let stats = cas.prune_unreferenced(false).unwrap();
        assert_eq!(stats.complete_removed, 1);
        assert_eq!(cas.package_count().unwrap(), 0);
    }

    #[test]
    fn prune_drops_staging_and_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("cas");
        let cas = CasCache::with_root(&cas_root);
        let pkg = tmp.path().join("pkg");
        write_pkg(&pkg, "ok");
        cas.store("keep-key", &pkg).unwrap();
        cas.link_to("keep-key", &tmp.path().join("vendor/keep"))
            .unwrap();

        let shard = cas_root.join("zz");
        let staging = shard.join("dead.staging");
        write_pkg(&staging.join("src"), "staging");
        let incomplete = shard.join("incompletehash");
        write_pkg(&incomplete, "no-marker");

        let stats = cas.prune_unreferenced(false).unwrap();
        assert_eq!(stats.complete_kept, 1);
        assert_eq!(stats.leftover_removed, 2);
        assert!(!staging.exists());
        assert!(!incomplete.exists());
        assert!(cas.contains("keep-key"));
    }
}
