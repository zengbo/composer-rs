//! Parallel HTTP downloads and archive extraction.

#![deny(unsafe_code)]

mod archive;
mod bins;
mod extract;

pub use archive::ArchiveType;
pub use bins::install_bins;
pub use extract::extract_archive;

use composer_auth::AuthStore;
use composer_cache::{CasCache, InstallKind, archives_dir};
use composer_core::error::{Error, Result};
use composer_lock::LockedPackage;
use composer_manifest::InstallerPaths;
use futures::stream::{FuturesUnordered, StreamExt};
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::{Digest as Sha2Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

const USER_AGENT: &str = concat!("composer-rs/", env!("CARGO_PKG_VERSION"));

/// Adaptive concurrency: `(cores × 8).clamp(16, 128)` — saturates bandwidth.
pub fn default_concurrency() -> usize {
    (num_cpus::get() * 8).clamp(16, 128)
}

/// Download / install statistics.
#[derive(Debug, Default)]
pub struct InstallStats {
    pub total: AtomicUsize,
    pub cache_hits: AtomicUsize,
    pub downloaded: AtomicUsize,
    pub failed: AtomicUsize,
    pub skipped: AtomicUsize,
    pub bytes: AtomicU64,
    pub hardlinks: AtomicU64,
    pub copies: AtomicU64,
}

impl InstallStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            total: self.total.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            downloaded: self.downloaded.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            hardlinks: self.hardlinks.load(Ordering::Relaxed),
            copies: self.copies.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub total: usize,
    pub cache_hits: usize,
    pub downloaded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub hardlinks: u64,
    pub copies: u64,
}

/// Parallel package installer: CAS + HTTP/2 downloads.
pub struct PackageInstaller {
    http: reqwest::Client,
    cache: CasCache,
    concurrency: usize,
    verify_checksums: bool,
    prefer_dist: bool,
    stats: Arc<InstallStats>,
    project_root: PathBuf,
    installer_paths: InstallerPaths,
    auth: AuthStore,
}

impl PackageInstaller {
    pub fn new(concurrency: usize, verify_checksums: bool) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .pool_max_idle_per_host(100)
            .http2_adaptive_window(true)
            .http2_initial_stream_window_size(4 * 1024 * 1024)
            .http2_initial_connection_window_size(8 * 1024 * 1024)
            .http2_keep_alive_interval(Duration::from_secs(15))
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| Error::other(e.to_string()))?;

        Ok(Self {
            http,
            cache: CasCache::new(),
            concurrency: concurrency.max(1),
            verify_checksums,
            prefer_dist: true,
            stats: Arc::new(InstallStats::default()),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            installer_paths: InstallerPaths::default(),
            auth: AuthStore::default(),
        })
    }

    pub fn with_auth(mut self, auth: AuthStore) -> Self {
        self.auth = auth;
        self
    }

    pub fn with_cache(mut self, cache: CasCache) -> Self {
        self.cache = cache;
        self
    }

    pub fn with_project_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.project_root = root.into();
        self
    }

    pub fn with_installer_paths(mut self, paths: InstallerPaths) -> Self {
        self.installer_paths = paths;
        self
    }

    pub fn with_prefer_dist(mut self, prefer_dist: bool) -> Self {
        self.prefer_dist = prefer_dist;
        self
    }

    pub fn stats(&self) -> Arc<InstallStats> {
        Arc::clone(&self.stats)
    }

    pub fn cache(&self) -> &CasCache {
        &self.cache
    }

    /// Install many locked packages into `vendor_dir` in parallel.
    pub async fn install_all(&self, packages: &[&LockedPackage], vendor_dir: &Path) -> Result<()> {
        let sem = Arc::new(Semaphore::new(self.concurrency));
        let mut futs = FuturesUnordered::new();

        self.stats.total.store(packages.len(), Ordering::Relaxed);

        for pkg in packages {
            if pkg.is_metapackage() {
                debug!(name = %pkg.name, "skipping metapackage");
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            let sem = Arc::clone(&sem);
            let http = self.http.clone();
            let cache = self.cache.clone();
            let stats = Arc::clone(&self.stats);
            let vendor_dir = vendor_dir.to_path_buf();
            let project_root = self.project_root.clone();
            let installer_paths = self.installer_paths.clone();
            let pkg = (*pkg).clone();
            let verify = self.verify_checksums;
            let prefer_dist = self.prefer_dist;
            let auth = self.auth.clone();

            futs.push(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");
                install_one(
                    &http,
                    &cache,
                    &stats,
                    &pkg,
                    &vendor_dir,
                    &project_root,
                    &installer_paths,
                    verify,
                    prefer_dist,
                    &auth,
                )
                .await
            });
        }

        let mut errors = Vec::new();
        while let Some(res) = futs.next().await {
            if let Err(e) = res {
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                errors.push(e);
            }
        }

        if !errors.is_empty() {
            let msg = errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            return Err(Error::other(format!(
                "{} package(s) failed to install:\n{msg}",
                errors.len()
            )));
        }
        Ok(())
    }
}

fn package_dest(
    pkg: &LockedPackage,
    vendor_dir: &Path,
    project_root: &Path,
    installer_paths: &InstallerPaths,
) -> Result<PathBuf> {
    if let Some(custom) =
        installer_paths.resolve(project_root, &pkg.name, pkg.package_type.as_deref())
    {
        return Ok(custom);
    }
    Ok(vendor_dir.join(pkg.package_id()?.install_path()))
}

async fn install_one(
    http: &reqwest::Client,
    cache: &CasCache,
    stats: &InstallStats,
    pkg: &LockedPackage,
    vendor_dir: &Path,
    project_root: &Path,
    installer_paths: &InstallerPaths,
    verify_checksums: bool,
    prefer_dist: bool,
    auth: &AuthStore,
) -> Result<()> {
    let dest = package_dest(pkg, vendor_dir, project_root, installer_paths)?;

    // Path repository: symlink or copy from local directory
    let result = if let Some(dist) = &pkg.dist {
        if dist.dist_type == "path" {
            install_path_package(pkg, &dest, Path::new(&dist.url), pkg.path_symlink(), stats)
        } else if !prefer_dist {
            if let Some(source) = &pkg.source {
                if source.source_type == "git" || source.source_type == "vcs" {
                    install_vcs_package(pkg, &dest, source, stats).await
                } else if pkg.dist_url().is_some() {
                    install_dist_package(http, cache, stats, pkg, &dest, verify_checksums, auth)
                        .await
                } else {
                    Err(Error::other(format!(
                        "package {} has no installable source",
                        pkg.name
                    )))
                }
            } else if pkg.dist_url().is_some() {
                install_dist_package(http, cache, stats, pkg, &dest, verify_checksums, auth).await
            } else {
                Err(Error::other(format!(
                    "package {} has no dist URL and no installable source",
                    pkg.name
                )))
            }
        } else if pkg.dist_url().is_some() {
            install_dist_package(http, cache, stats, pkg, &dest, verify_checksums, auth).await
        } else if let Some(source) = &pkg.source {
            if source.source_type == "git" || source.source_type == "vcs" {
                install_vcs_package(pkg, &dest, source, stats).await
            } else {
                Err(Error::other(format!(
                    "package {} has no dist URL and no installable source",
                    pkg.name
                )))
            }
        } else {
            Err(Error::other(format!(
                "package {} has no dist URL and no installable source",
                pkg.name
            )))
        }
    } else if let Some(source) = &pkg.source {
        if source.source_type == "path" {
            install_path_package(
                pkg,
                &dest,
                Path::new(&source.url),
                pkg.path_symlink(),
                stats,
            )
        } else if source.source_type == "git" || source.source_type == "vcs" {
            install_vcs_package(pkg, &dest, source, stats).await
        } else {
            Err(Error::other(format!(
                "package {} has no dist URL and no installable source",
                pkg.name
            )))
        }
    } else {
        Err(Error::other(format!(
            "package {} has no dist URL and no installable source",
            pkg.name
        )))
    };

    if result.is_ok() {
        write_install_marker(pkg, &dest)?;
    }
    result
}

/// Marker written into each package dir for `composer-rs status` integrity checks.
pub fn write_install_marker(pkg: &LockedPackage, dest: &Path) -> Result<()> {
    if !dest.exists() {
        return Ok(());
    }
    // Symlinked path packages: still write marker next to target if possible
    let marker = dest.join(".composer-rs-installed");
    let body = serde_json::json!({
        "name": pkg.name,
        "version": pkg.version,
        "cache_key": pkg.cache_key(),
        "reference": pkg.dist.as_ref().and_then(|d| d.reference.clone())
            .or_else(|| pkg.source.as_ref().and_then(|s| s.reference.clone())),
    });
    std::fs::write(&marker, body.to_string() + "\n").map_err(|e| Error::io(&marker, e))?;
    Ok(())
}

async fn install_dist_package(
    http: &reqwest::Client,
    cache: &CasCache,
    stats: &InstallStats,
    pkg: &LockedPackage,
    dest: &Path,
    verify_checksums: bool,
    auth: &AuthStore,
) -> Result<()> {
    let key = pkg.cache_key();

    // Fast path: CAS hit → hardlink into vendor
    if cache.contains(&key) {
        let link = cache.link_to(&key, dest)?;
        stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        stats.hardlinks.fetch_add(link.hardlinks, Ordering::Relaxed);
        stats.copies.fetch_add(link.copies, Ordering::Relaxed);
        debug!(name = %pkg.name, "CAS hit");
        return Ok(());
    }

    let urls = dist_urls(pkg);
    if urls.is_empty() {
        return Err(Error::other(format!(
            "package {} has no dist URL",
            pkg.name
        )));
    }

    let mut last_err = None;
    let mut archive_path = None;
    for url in &urls {
        match download_to_archives(http, url, &pkg.name, auth).await {
            Ok(p) => {
                archive_path = Some(p);
                break;
            }
            Err(e) => {
                warn!(package = %pkg.name, %url, error = %e, "dist download failed, trying next mirror");
                last_err = Some(e);
            }
        }
    }
    let archive_path = archive_path.ok_or_else(|| {
        last_err.unwrap_or_else(|| Error::other(format!("package {} has no dist URL", pkg.name)))
    })?;
    let bytes = std::fs::metadata(&archive_path)
        .map(|m| m.len())
        .unwrap_or(0);
    stats.bytes.fetch_add(bytes, Ordering::Relaxed);

    if verify_checksums {
        if let Some(dist) = &pkg.dist {
            if let Some(expected) = &dist.shasum {
                if !expected.is_empty() {
                    verify_sha1(&archive_path, expected, &pkg.name)?;
                }
            }
        }
    }

    // Extract to temp, store in CAS, hardlink to vendor
    let tmp = tempfile::tempdir().map_err(|e| Error::Cache(e.to_string()))?;
    let extract_root = tmp.path().join("extracted");
    std::fs::create_dir_all(&extract_root).map_err(|e| Error::io(&extract_root, e))?;

    extract_archive(&archive_path, &extract_root)?;

    // Composer zips usually have a single top-level directory — unwrap it.
    let package_root = unwrap_single_root(&extract_root)?;

    cache.store(&key, &package_root)?;
    let link = cache.link_to(&key, &dest)?;
    stats.downloaded.fetch_add(1, Ordering::Relaxed);
    stats.hardlinks.fetch_add(link.hardlinks, Ordering::Relaxed);
    stats.copies.fetch_add(link.copies, Ordering::Relaxed);

    debug!(name = %pkg.name, "downloaded and cached");
    let _ = InstallKind::CacheMiss {
        hardlinks: link.hardlinks,
        copies: link.copies,
    };
    Ok(())
}

/// Primary dist URL plus any `dist.mirrors` entries (Composer failover list).
fn dist_urls(pkg: &LockedPackage) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(u) = pkg.dist_url() {
        urls.push(u.to_string());
    }
    if let Some(dist) = &pkg.dist {
        if let Some(mirrors) = &dist.mirrors {
            for m in mirrors {
                if let Some(u) = m.as_str() {
                    if !urls.iter().any(|x| x == u) {
                        urls.push(u.to_string());
                    }
                } else if let Some(u) = m.get("url").and_then(|v| v.as_str()) {
                    if !urls.iter().any(|x| x == u) {
                        urls.push(u.to_string());
                    }
                }
            }
        }
    }
    urls
}

async fn download_to_archives(
    http: &reqwest::Client,
    url: &str,
    package_name: &str,
    auth: &AuthStore,
) -> Result<PathBuf> {
    let dir = archives_dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::io(dir.as_path(), e))?;

    let hash = blake3::hash(url.as_bytes());
    let ext = guess_extension(url);
    let path = dir.join(format!("{}.{}", hash.to_hex(), ext));

    if path.is_file() {
        debug!(%url, "archive already on disk");
        return Ok(path);
    }

    let partial = path.with_extension(format!("{ext}.partial"));
    let mut attempt = 0;
    let max_attempts = 4;

    loop {
        attempt += 1;
        match download_file(http, url, &partial, auth).await {
            Ok(()) => {
                std::fs::rename(&partial, &path).map_err(|e| Error::io(&path, e))?;
                return Ok(path);
            }
            Err(e) if attempt < max_attempts => {
                warn!(
                    package = %package_name,
                    attempt,
                    error = %e,
                    "download failed, retrying"
                );
                let backoff = Duration::from_millis(200 * 2u64.pow(attempt - 1));
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn download_file(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
    auth: &AuthStore,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    let builder = auth.apply_to_request(url, http.get(url));
    let resp = builder
        .send()
        .await
        .map_err(|e| Error::download(url, e.to_string()))?;

    if !resp.status().is_success() {
        return Err(Error::download(url, format!("HTTP {}", resp.status())));
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| Error::io(dest, e))?;

    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::download(url, e.to_string()))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| Error::io(dest, e))?;
    }
    file.flush().await.map_err(|e| Error::io(dest, e))?;
    Ok(())
}

fn verify_sha1(path: &Path, expected: &str, package: &str) -> Result<()> {
    let data = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = Sha1::new();
    Sha1Digest::update(&mut hasher, &data);
    let actual = hex::encode(Sha1Digest::finalize(hasher));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(Error::ChecksumMismatch {
            package: package.into(),
            expected: expected.into(),
            actual,
        });
    }
    Ok(())
}

/// Optional SHA-256 verify helper.
pub fn verify_sha256(path: &Path, expected: &str, package: &str) -> Result<()> {
    let data = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = Sha256::new();
    Sha2Digest::update(&mut hasher, &data);
    let actual = hex::encode(Sha2Digest::finalize(hasher));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(Error::ChecksumMismatch {
            package: package.into(),
            expected: expected.into(),
            actual,
        });
    }
    Ok(())
}

fn guess_extension(url: &str) -> &'static str {
    let lower = url.to_ascii_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        "tar.gz"
    } else if path.ends_with(".tar.bz2") {
        "tar.bz2"
    } else if path.ends_with(".tar.xz") {
        "tar.xz"
    } else if path.ends_with(".tar") {
        "tar"
    } else {
        "zip"
    }
}

fn install_path_package(
    pkg: &LockedPackage,
    dest: &Path,
    src: &Path,
    symlink: bool,
    stats: &InstallStats,
) -> Result<()> {
    if !src.exists() {
        return Err(Error::other(format!(
            "path package {} source missing: {}",
            pkg.name,
            src.display()
        )));
    }
    if dest.exists() {
        if dest.is_symlink() || dest.is_file() {
            std::fs::remove_file(dest).map_err(|e| Error::io(dest, e))?;
        } else {
            std::fs::remove_dir_all(dest).map_err(|e| Error::io(dest, e))?;
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    #[cfg(unix)]
    if symlink {
        std::os::unix::fs::symlink(src, dest).map_err(|e| Error::io(dest, e))?;
        stats.cache_hits.fetch_add(1, Ordering::Relaxed);
        stats.hardlinks.fetch_add(1, Ordering::Relaxed);
        debug!(name = %pkg.name, src = %src.display(), "symlinked path package");
        return Ok(());
    }

    // Copy tree
    copy_dir(src, dest)?;
    stats.downloaded.fetch_add(1, Ordering::Relaxed);
    stats.copies.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

async fn install_vcs_package(
    pkg: &LockedPackage,
    dest: &Path,
    source: &composer_lock::SourceInfo,
    stats: &InstallStats,
) -> Result<()> {
    // Prefer a cache checkout keyed by URL; fall back to fresh clone into dest.
    let cache_key = format!(
        "vcs:{}@{}",
        source.url,
        source.reference.as_deref().unwrap_or("HEAD")
    );
    let hash = blake3::hash(cache_key.as_bytes());
    let cache_dir = composer_cache::cache_root()
        .join("vcs")
        .join(hash.to_hex().as_str());

    if !cache_dir.join(".git").is_dir() {
        if cache_dir.exists() {
            std::fs::remove_dir_all(&cache_dir).map_err(|e| Error::io(&cache_dir, e))?;
        }
        if let Some(parent) = cache_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        // Only pass --branch for branch/tag names. Commit SHAs are not valid
        // --branch values and would make shallow clone fail.
        let mut clone = tokio::process::Command::new("git");
        clone.args(["clone", "--depth", "1"]);
        if let Some(reference) = &source.reference {
            if is_git_branch_or_tag(reference) {
                clone.args(["--branch", reference]);
            }
        }
        clone.arg(&source.url).arg(&cache_dir);

        let status = clone
            .status()
            .await
            .map_err(|e| Error::other(format!("git clone: {e}")))?;
        if !status.success() {
            return Err(Error::other(format!("git clone failed for {}", source.url)));
        }

        if let Some(reference) = &source.reference {
            // For SHAs, a shallow clone of default branch may not contain the
            // commit — fetch it explicitly when checkout fails.
            let checkout = tokio::process::Command::new("git")
                .args(["-C"])
                .arg(&cache_dir)
                .args(["checkout", reference])
                .status()
                .await
                .map_err(|e| Error::other(format!("git checkout: {e}")))?;
            if !checkout.success() {
                let fetch = tokio::process::Command::new("git")
                    .args(["-C"])
                    .arg(&cache_dir)
                    .args(["fetch", "--depth", "1", "origin", reference])
                    .status()
                    .await
                    .map_err(|e| Error::other(format!("git fetch: {e}")))?;
                if !fetch.success() {
                    return Err(Error::other(format!(
                        "git fetch {reference} failed for {}",
                        source.url
                    )));
                }
                let checkout2 = tokio::process::Command::new("git")
                    .args(["-C"])
                    .arg(&cache_dir)
                    .args(["checkout", "FETCH_HEAD"])
                    .status()
                    .await
                    .map_err(|e| Error::other(format!("git checkout: {e}")))?;
                if !checkout2.success() {
                    return Err(Error::other(format!(
                        "git checkout {reference} failed for {}",
                        source.url
                    )));
                }
            }
        }
    }

    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| Error::io(dest, e))?;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    // Copy working tree (exclude .git for a cleaner vendor)
    copy_dir_excluding_git(&cache_dir, dest)?;
    stats.downloaded.fetch_add(1, Ordering::Relaxed);
    stats.copies.fetch_add(1, Ordering::Relaxed);
    debug!(name = %pkg.name, "installed from vcs");
    Ok(())
}

/// True for refs safe to pass as `git clone --branch` (not a bare commit SHA).
fn is_git_branch_or_tag(reference: &str) -> bool {
    let r = reference.trim();
    if r.is_empty() {
        return false;
    }
    // 7–40 hex chars → treat as commit-ish, not a branch name.
    if (7..=40).contains(&r.len()) && r.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    true
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| Error::io(dst, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| Error::io(src, e))? {
        let entry = entry.map_err(|e| Error::io(src, e))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type().map_err(|e| Error::io(&from, e))?;
        if ft.is_dir() {
            copy_dir(&from, &to)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to).map_err(|e| Error::io(&to, e))?;
        }
    }
    Ok(())
}

fn copy_dir_excluding_git(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| Error::io(dst, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| Error::io(src, e))? {
        let entry = entry.map_err(|e| Error::io(src, e))?;
        if entry.file_name() == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type().map_err(|e| Error::io(&from, e))?;
        if ft.is_dir() {
            copy_dir_excluding_git(&from, &to)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to).map_err(|e| Error::io(&to, e))?;
        }
    }
    Ok(())
}

/// If extract root contains a single directory, return that; else root itself.
fn unwrap_single_root(extract_root: &Path) -> Result<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(extract_root)
        .map_err(|e| Error::io(extract_root, e))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            name != "." && name != ".."
        })
        .collect();

    if entries.len() == 1 {
        let only = entries.remove(0);
        if only.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            return Ok(only.path());
        }
    }
    Ok(extract_root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use composer_lock::{DistInfo, LockedPackage};
    use std::collections::BTreeMap;

    fn pkg_with_mirrors(url: &str, mirrors: Option<Vec<serde_json::Value>>) -> LockedPackage {
        LockedPackage {
            name: "acme/lib".into(),
            version: "1.0.0".into(),
            source: None,
            dist: Some(DistInfo {
                dist_type: "zip".into(),
                url: url.into(),
                reference: Some("abc".into()),
                shasum: None,
                mirrors,
            }),
            require: BTreeMap::new(),
            require_dev: BTreeMap::new(),
            package_type: Some("library".into()),
            extra: None,
            autoload: None,
            autoload_dev: None,
            notification_url: None,
            license: vec![],
            description: None,
            homepage: None,
            keywords: vec![],
            time: None,
            replace: BTreeMap::new(),
            provide: BTreeMap::new(),
            conflict: BTreeMap::new(),
            suggest: BTreeMap::new(),
            bin: vec![],
            abandoned: None,
        }
    }

    #[test]
    fn dist_urls_primary_then_string_and_object_mirrors() {
        let pkg = pkg_with_mirrors(
            "https://primary.example/a.zip",
            Some(vec![
                serde_json::json!("https://mirror.example/a.zip"),
                serde_json::json!({"url": "https://mirror2.example/a.zip"}),
                serde_json::json!("https://primary.example/a.zip"),
            ]),
        );
        assert_eq!(
            dist_urls(&pkg),
            vec![
                "https://primary.example/a.zip",
                "https://mirror.example/a.zip",
                "https://mirror2.example/a.zip",
            ]
        );
    }

    #[test]
    fn dist_urls_without_mirrors_is_just_primary() {
        let pkg = pkg_with_mirrors("https://primary.example/a.zip", None);
        assert_eq!(dist_urls(&pkg), vec!["https://primary.example/a.zip"]);
    }
}
