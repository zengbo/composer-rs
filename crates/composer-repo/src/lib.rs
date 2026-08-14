//! Packagist and Composer repository API client.

#![deny(unsafe_code)]

use composer_auth::AuthStore;
use composer_cache::metadata_dir;
use composer_core::error::{Error, Result};
use composer_core::{AutoloadConfig, ComposerVersion, PackageId, VersionConstraint};
use composer_lock::{DistInfo, LockedPackage, SourceInfo};
use dashmap::DashMap;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::debug;

const DEFAULT_PACKAGIST: &str = "https://repo.packagist.org";
const USER_AGENT: &str = concat!("composer-rs/", env!("CARGO_PKG_VERSION"));

/// Package repository client (Packagist Composer v2 API).
#[derive(Clone)]
pub struct RepositoryClient {
    http: reqwest::Client,
    base_url: String,
    /// In-memory package metadata cache.
    memory: Arc<DashMap<String, CachedPackage>>,
    metadata_ttl: Duration,
    auth: AuthStore,
    /// Composer `config.secure-http` (default true).
    secure_http: bool,
}

#[derive(Clone)]
struct CachedPackage {
    versions: Vec<RemotePackageVersion>,
    fetched_at: SystemTime,
}

/// A single version available from the repository.
#[derive(Debug, Clone)]
pub struct RemotePackageVersion {
    pub name: String,
    pub version: ComposerVersion,
    pub version_normalized: String,
    pub dist: Option<DistInfo>,
    pub source: Option<SourceInfo>,
    pub require: BTreeMap<String, String>,
    pub require_dev: BTreeMap<String, String>,
    pub package_type: Option<String>,
    pub autoload: Option<AutoloadConfig>,
    pub autoload_dev: Option<AutoloadConfig>,
    pub provide: BTreeMap<String, String>,
    pub replace: BTreeMap<String, String>,
    pub conflict: BTreeMap<String, String>,
    pub bin: Vec<String>,
    pub description: Option<String>,
    pub license: Vec<String>,
    pub abandoned: Option<serde_json::Value>,
    pub time: Option<String>,
}

impl RemotePackageVersion {
    pub fn to_locked(&self) -> LockedPackage {
        LockedPackage {
            name: self.name.clone(),
            version: self.version.raw.clone(),
            source: self.source.clone(),
            dist: self.dist.clone(),
            require: self.require.clone(),
            require_dev: self.require_dev.clone(),
            package_type: self.package_type.clone(),
            extra: None,
            autoload: self.autoload.clone(),
            autoload_dev: self.autoload_dev.clone(),
            notification_url: Some("https://packagist.org/downloads/".into()),
            license: self
                .license
                .iter()
                .map(|l| composer_lock::License::One(l.clone()))
                .collect(),
            description: self.description.clone(),
            homepage: None,
            keywords: vec![],
            time: self.time.clone(),
            replace: self.replace.clone(),
            provide: self.provide.clone(),
            conflict: self.conflict.clone(),
            suggest: BTreeMap::new(),
            bin: self.bin.clone(),
            abandoned: self.abandoned.clone(),
            unknown: BTreeMap::new(),
        }
    }

    pub fn non_platform_requires(&self) -> Vec<(PackageId, VersionConstraint)> {
        self.require
            .iter()
            .filter_map(|(name, c)| {
                let id = PackageId::parse(name).ok()?;
                if id.is_platform() {
                    return None;
                }
                Some((id, VersionConstraint::new(c.clone())))
            })
            .collect()
    }
}

impl RepositoryClient {
    pub fn new() -> Result<Self> {
        Self::with_base_url(DEFAULT_PACKAGIST)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        Self::with_base_url_auth(base_url, AuthStore::default())
    }

    pub fn with_base_url_auth(base_url: impl Into<String>, auth: AuthStore) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .pool_max_idle_per_host(32)
            .http2_adaptive_window(true)
            .http2_initial_stream_window_size(2 * 1024 * 1024)
            .http2_initial_connection_window_size(4 * 1024 * 1024)
            .http2_keep_alive_interval(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| Error::other(e.to_string()))?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            memory: Arc::new(DashMap::new()),
            metadata_ttl: Duration::from_secs(600),
            auth,
            secure_http: true,
        })
    }

    pub fn with_auth(mut self, auth: AuthStore) -> Self {
        self.auth = auth;
        self
    }

    pub fn with_secure_http(mut self, secure_http: bool) -> Self {
        self.secure_http = secure_http;
        self
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Fetch all versions of a package (Composer API v2).
    pub async fn get_package_versions(
        &self,
        name: &PackageId,
    ) -> Result<Vec<RemotePackageVersion>> {
        let key = self.metadata_identity(name.as_str());

        if let Some(cached) = self.memory.get(&key) {
            if cached.fetched_at.elapsed().unwrap_or(Duration::MAX) < self.metadata_ttl {
                return Ok(cached.versions.clone());
            }
        }

        // Disk cache (scoped by repository origin + auth, not package name alone)
        if let Some(versions) = self.load_disk_cache(name.as_str())? {
            self.memory.insert(
                key.clone(),
                CachedPackage {
                    versions: versions.clone(),
                    fetched_at: SystemTime::now(),
                },
            );
            return Ok(versions);
        }

        let url = format!("{}/p2/{}.json", self.base_url, name.as_str());
        debug!(%url, "fetching package metadata");

        if is_insecure_http(&url) && self.secure_http {
            return Err(Error::download(
                &url,
                "refusing insecure HTTP URL (set config.secure-http=false to allow)",
            ));
        }

        let builder = self.auth.apply_to_request(&url, self.http.get(&url));
        let resp = builder
            .send()
            .await
            .map_err(|e| Error::download(&url, e.to_string()))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::PackageNotFound(name.to_string()));
        }

        if !resp.status().is_success() {
            return Err(Error::download(&url, format!("HTTP {}", resp.status())));
        }

        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::download(&url, e.to_string()))?;

        let versions = parse_p2_response(name.as_str(), &body)?;
        self.save_disk_cache(name.as_str(), &body)?;

        self.memory.insert(
            key,
            CachedPackage {
                versions: versions.clone(),
                fetched_at: SystemTime::now(),
            },
        );

        Ok(versions)
    }

    /// Best matching version for constraint (prefer stable, then highest).
    pub async fn find_best(
        &self,
        name: &PackageId,
        constraint: &VersionConstraint,
        prefer_stable: bool,
        min_stability: &str,
    ) -> Result<RemotePackageVersion> {
        let versions = self.get_package_versions(name).await?;
        let min = parse_min_stability(min_stability);

        let mut candidates: Vec<&RemotePackageVersion> = versions
            .iter()
            .filter(|v| {
                v.version.stability() as u8 >= min as u8 || constraint.as_str().contains("dev")
            })
            .filter(|v| constraint.matches(&v.version))
            .collect();

        if candidates.is_empty() {
            return Err(Error::NoMatchingVersion {
                package: name.to_string(),
                constraint: constraint.to_string(),
            });
        }

        if prefer_stable {
            let stable: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|v| v.version.is_stable())
                .collect();
            if !stable.is_empty() {
                candidates = stable;
            }
        }

        candidates.sort_by(|a, b| b.version.cmp(&a.version));
        Ok(candidates[0].clone())
    }

    /// Search packages via the Packagist-compatible search API for this repository base URL.
    ///
    /// Applies configured auth (http-basic / bearer / tokens) so private Packagist /
    /// Satis mirrors that protect `/search.json` work the same as metadata fetches.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let base = self.base_url.trim_end_matches('/');
        // Public packagist uses packagist.org (not repo.packagist.org) for search.
        let search_base = if base.contains("repo.packagist.org") || base.contains("packagist.org") {
            "https://packagist.org"
        } else {
            base
        };
        let url = format!(
            "{search_base}/search.json?q={}&per_page={}",
            urlencoding_minimal(query),
            limit
        );
        let builder = self.auth.apply_to_request(&url, self.http.get(&url));
        let resp = builder
            .send()
            .await
            .map_err(|e| Error::download(&url, e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Error::download(&url, format!("HTTP {}", resp.status())));
        }
        let body: SearchResponse = resp
            .json()
            .await
            .map_err(|e| Error::download(&url, e.to_string()))?;
        Ok(body.results)
    }

    /// Package info for `show` command.
    pub async fn show(&self, name: &PackageId) -> Result<Vec<RemotePackageVersion>> {
        self.get_package_versions(name).await
    }

    fn auth_fingerprint(&self) -> String {
        match self.auth.for_url(&self.base_url) {
            None => "anon".into(),
            Some(auth) => blake3::hash(format!("{auth:?}").as_bytes())
                .to_hex()
                .to_string(),
        }
    }

    fn metadata_identity(&self, package: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.base_url.as_bytes());
        hasher.update(&[0]);
        hasher.update(package.as_bytes());
        hasher.update(&[0]);
        hasher.update(self.auth_fingerprint().as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    fn disk_path(&self, package: &str) -> PathBuf {
        let id = self.metadata_identity(package);
        metadata_dir()
            .join("p2")
            .join(&id[..2])
            .join(format!("{id}.json"))
    }

    fn load_disk_cache(&self, package: &str) -> Result<Option<Vec<RemotePackageVersion>>> {
        let path = self.disk_path(package);
        if !path.is_file() {
            return Ok(None);
        }
        // TTL: 10 minutes
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if modified.elapsed().unwrap_or(Duration::MAX) > self.metadata_ttl {
                    return Ok(None);
                }
            }
        }
        let body = fs::read(&path).map_err(|e| Error::io(&path, e))?;
        Ok(Some(parse_p2_response(package, &body)?))
    }

    fn save_disk_cache(&self, package: &str, body: &[u8]) -> Result<()> {
        let path = self.disk_path(package);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        fs::write(&path, body).map_err(|e| Error::io(&path, e))?;
        Ok(())
    }
}

impl Default for RepositoryClient {
    fn default() -> Self {
        Self::new().expect("failed to create default HTTP client")
    }
}

/// Ordered registry of Composer repositories (custom repos first, then Packagist).
#[derive(Clone)]
pub struct RepositoryRegistry {
    clients: Vec<RepositoryClient>,
}

impl RepositoryRegistry {
    /// Build from `composer.json` repositories using **global/env auth only**.
    ///
    /// Prefer [`from_manifest_auth`] with `AuthStore::load(Some(project_root))` so
    /// project-local `auth.json` is applied (required for private Composer repos).
    #[deprecated(
        note = "use RepositoryRegistry::from_manifest_auth(manifest, AuthStore::load(Some(project_root)))"
    )]
    pub fn from_manifest(manifest: &composer_manifest::ComposerJson) -> Result<Self> {
        let auth = AuthStore::load(None).unwrap_or_default();
        Self::from_manifest_auth(manifest, auth)
    }

    pub fn from_manifest_auth(
        manifest: &composer_manifest::ComposerJson,
        auth: AuthStore,
    ) -> Result<Self> {
        let repos = manifest.repositories_list();
        let mut clients = Vec::new();
        let mut has_packagist = false;
        let secure_http = manifest.secure_http();

        for repo in &repos {
            if let composer_manifest::Repository::Composer { url } = repo {
                if secure_http && is_insecure_http(url) {
                    return Err(Error::other(format!(
                        "refusing insecure repository URL `{url}` \
                         (set config.secure-http=false to allow HTTP)"
                    )));
                }
                let normalized = url.trim_end_matches('/');
                if normalized.contains("packagist.org") {
                    has_packagist = true;
                }
                clients.push(
                    RepositoryClient::with_base_url_auth(url, auth.clone())?
                        .with_secure_http(secure_http),
                );
            }
        }

        if manifest.packagist_enabled() && !has_packagist {
            clients.push(RepositoryClient::with_base_url_auth(
                DEFAULT_PACKAGIST,
                auth,
            )?);
        }

        Ok(Self { clients })
    }

    /// Number of HTTP repository clients (0 when only path/VCS/inline repos remain).
    pub fn repository_count(&self) -> usize {
        self.clients.len()
    }

    /// Fetch package versions, trying each repository in order.
    pub async fn get_package_versions(
        &self,
        name: &PackageId,
    ) -> Result<Vec<RemotePackageVersion>> {
        let mut last_not_found = None;
        for client in &self.clients {
            match client.get_package_versions(name).await {
                Ok(versions) if !versions.is_empty() => return Ok(versions),
                Ok(_) => continue,
                Err(Error::PackageNotFound(_)) => {
                    last_not_found = Some(Error::PackageNotFound(name.to_string()));
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_not_found.unwrap_or_else(|| Error::PackageNotFound(name.to_string())))
    }

    /// Search across configured repositories (first successful non-empty result wins;
    /// falls back to the last error if all fail).
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let mut last_err = None;
        for client in &self.clients {
            match client.search(query, limit).await {
                Ok(results) if !results.is_empty() => return Ok(results),
                Ok(empty) => {
                    // Keep empty success as fallback if nothing else returns hits.
                    if last_err.is_none() {
                        return Ok(empty);
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::other("no repository configured")))
    }

    /// Delegate show to the registry (tries all repos).
    pub async fn show(&self, name: &PackageId) -> Result<Vec<RemotePackageVersion>> {
        self.get_package_versions(name).await
    }

    /// Best matching version across all repositories.
    pub async fn find_best(
        &self,
        name: &PackageId,
        constraint: &VersionConstraint,
        prefer_stable: bool,
        min_stability: &str,
    ) -> Result<RemotePackageVersion> {
        let mut last_no_match = None;
        for client in &self.clients {
            match client
                .find_best(name, constraint, prefer_stable, min_stability)
                .await
            {
                Ok(v) => return Ok(v),
                Err(Error::NoMatchingVersion { .. }) => {
                    last_no_match = Some(Error::NoMatchingVersion {
                        package: name.to_string(),
                        constraint: constraint.to_string(),
                    });
                }
                Err(Error::PackageNotFound(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(last_no_match.unwrap_or_else(|| Error::PackageNotFound(name.to_string())))
    }
}

fn is_insecure_http(url: &str) -> bool {
    url.trim().len() >= 7 && url.trim()[..7].eq_ignore_ascii_case("http://")
}

fn parse_min_stability(s: &str) -> composer_core::version::Stability {
    composer_core::version::Stability::parse(s)
}

/// Minimal URL encoding for search queries.
fn urlencoding_minimal(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".into(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResult {
    pub name: String,
    pub description: Option<String>,
    pub url: Option<String>,
    pub repository: Option<String>,
    pub downloads: Option<u64>,
    pub favers: Option<u64>,
}

fn parse_p2_response(package: &str, body: &[u8]) -> Result<Vec<RemotePackageVersion>> {
    let parsed: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Error::other(format!("p2 parse: {e}")))?;
    let packages = parsed
        .get("packages")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let list = packages
        .get(package)
        .cloned()
        .or_else(|| packages.values().next().cloned())
        .unwrap_or(serde_json::Value::Array(vec![]));

    // Composer v2 p2 is an array of version objects. Some hosts (GitLab /
    // Satis variants) emit a version-keyed object instead.
    let items = match list {
        serde_json::Value::Array(arr) => arr,
        serde_json::Value::Object(map) => map
            .into_iter()
            .map(|(ver, mut v)| {
                if let Some(obj) = v.as_object_mut() {
                    obj.entry("version")
                        .or_insert(serde_json::Value::String(ver));
                }
                v
            })
            .collect(),
        _ => Vec::new(),
    };

    let mut versions = Vec::with_capacity(items.len());
    for item in items {
        if let Some(v) = remote_version_from_p2(package, &item) {
            versions.push(v);
        }
    }
    Ok(versions)
}

/// GitLab / Satis sometimes emit a JSON array where Composer documents a
/// string (`description`, `dist.url`, …). Take the first scalar rather than
/// failing the whole p2 document.
fn json_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if s != "__unset" => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Array(arr) => arr.iter().find_map(json_string),
        _ => None,
    }
}

fn json_string_map(value: &serde_json::Value) -> BTreeMap<String, String> {
    let Some(obj) = value.as_object() else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        if let Some(s) = json_string(v) {
            out.insert(k.clone(), s);
        }
    }
    out
}

fn json_string_list(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => arr.iter().filter_map(json_string).collect(),
        _ => Vec::new(),
    }
}

fn json_object<'a>(
    value: &'a serde_json::Value,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    value.as_object()
}

fn parse_p2_dist(value: &serde_json::Value) -> Option<DistInfo> {
    let obj = json_object(value)?;
    let dist_type = obj.get("type").and_then(json_string)?;
    let (url, extra_mirrors) = match obj.get("url") {
        Some(serde_json::Value::Array(arr)) if !arr.is_empty() => {
            let url = json_string(&arr[0])?;
            let rest = arr[1..].iter().cloned().collect::<Vec<_>>();
            (url, rest)
        }
        Some(v) => (json_string(v)?, Vec::new()),
        None => return None,
    };
    let mut mirrors = match obj.get("mirrors") {
        Some(serde_json::Value::Array(arr)) => Some(arr.clone()),
        Some(other) => Some(vec![other.clone()]),
        None => None,
    };
    if !extra_mirrors.is_empty() {
        mirrors.get_or_insert_with(Vec::new).extend(extra_mirrors);
    }
    Some(DistInfo {
        dist_type,
        url,
        reference: obj.get("reference").and_then(json_string),
        shasum: obj.get("shasum").and_then(json_string),
        mirrors,
    })
}

fn parse_p2_source(value: &serde_json::Value) -> Option<SourceInfo> {
    let obj = json_object(value)?;
    Some(SourceInfo {
        source_type: obj.get("type").and_then(json_string)?,
        url: obj.get("url").and_then(json_string)?,
        reference: obj.get("reference").and_then(json_string),
    })
}

fn parse_p2_autoload(value: &serde_json::Value) -> Option<AutoloadConfig> {
    serde_json::from_value(value.clone()).ok()
}

fn remote_version_from_p2(package: &str, item: &serde_json::Value) -> Option<RemotePackageVersion> {
    let obj = json_object(item)?;
    let raw_version = obj.get("version").and_then(json_string)?;
    let version = ComposerVersion::parse(&raw_version).ok()?;
    let version_normalized = obj
        .get("version_normalized")
        .and_then(json_string)
        .unwrap_or_default();
    Some(RemotePackageVersion {
        name: package.to_string(),
        version,
        version_normalized,
        dist: obj.get("dist").and_then(parse_p2_dist),
        source: obj.get("source").and_then(parse_p2_source),
        require: obj.get("require").map(json_string_map).unwrap_or_default(),
        require_dev: obj
            .get("require-dev")
            .map(json_string_map)
            .unwrap_or_default(),
        package_type: obj.get("type").and_then(json_string),
        autoload: obj.get("autoload").and_then(parse_p2_autoload),
        autoload_dev: obj.get("autoload-dev").and_then(parse_p2_autoload),
        provide: obj.get("provide").map(json_string_map).unwrap_or_default(),
        replace: obj.get("replace").map(json_string_map).unwrap_or_default(),
        conflict: obj.get("conflict").map(json_string_map).unwrap_or_default(),
        bin: obj.get("bin").map(json_string_list).unwrap_or_default(),
        description: obj.get("description").and_then(json_string),
        license: obj.get("license").map(json_string_list).unwrap_or_default(),
        abandoned: obj.get("abandoned").cloned(),
        time: obj.get("time").and_then(json_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_p2() {
        let sample = r#"{
            "packages": {
                "psr/log": [
                    {
                        "version": "3.0.0",
                        "version_normalized": "3.0.0.0",
                        "dist": {
                            "type": "zip",
                            "url": "https://api.github.com/repos/php-fig/log/zipball/fe5ea303b0887d5caefd3d431c3e61ad47037001",
                            "reference": "fe5ea303b0887d5caefd3d431c3e61ad47037001"
                        },
                        "require": { "php": ">=8.0.0" },
                        "type": "library",
                        "autoload": {
                            "psr-4": { "Psr\\Log\\": "src" }
                        }
                    }
                ]
            }
        }"#;
        let versions = parse_p2_response("psr/log", sample.as_bytes()).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version.raw, "3.0.0");
        assert!(versions[0].dist.is_some());
    }

    #[test]
    fn parse_p2_preserves_dist_mirrors_string_and_object() {
        let sample = r#"{
            "packages": {
                "acme/lib": [
                    {
                        "version": "1.0.0",
                        "dist": {
                            "type": "zip",
                            "url": "https://primary.example/acme.zip",
                            "reference": "abc",
                            "mirrors": [
                                "https://mirror.example/acme.zip",
                                { "url": "https://mirror2.example/acme.zip" }
                            ]
                        }
                    }
                ]
            }
        }"#;
        let versions = parse_p2_response("acme/lib", sample.as_bytes()).unwrap();
        let dist = versions[0].dist.as_ref().expect("dist");
        let mirrors = dist.mirrors.as_ref().expect("mirrors");
        assert_eq!(mirrors.len(), 2);
        assert_eq!(mirrors[0].as_str(), Some("https://mirror.example/acme.zip"));
        assert_eq!(
            mirrors[1].get("url").and_then(|v| v.as_str()),
            Some("https://mirror2.example/acme.zip")
        );
        let locked = versions[0].to_locked();
        assert_eq!(locked.dist.as_ref().unwrap().mirrors, dist.mirrors);
    }

    #[test]
    fn parse_p2_accepts_array_scalars_used_by_gitlab() {
        let sample = r#"{
            "packages": {
                "acme/lib": [
                    {
                        "version": "1.0.0",
                        "description": ["line one", "line two"],
                        "type": ["library"],
                        "time": ["2024-01-01T00:00:00+00:00"],
                        "dist": {
                            "type": "zip",
                            "url": [
                                "https://gitlab.example/a.zip",
                                "https://gitlab.example/b.zip"
                            ],
                            "reference": ["abc"]
                        },
                        "require": { "php": [">=8.1"] },
                        "autoload": {
                            "classmap": "src/Foo.php",
                            "psr-4": { "Acme\\": ["src/", "lib/"] }
                        }
                    }
                ]
            }
        }"#;
        let versions = parse_p2_response("acme/lib", sample.as_bytes()).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].description.as_deref(), Some("line one"));
        assert_eq!(versions[0].package_type.as_deref(), Some("library"));
        let dist = versions[0].dist.as_ref().expect("dist");
        assert_eq!(dist.url, "https://gitlab.example/a.zip");
        assert_eq!(dist.reference.as_deref(), Some("abc"));
        assert_eq!(
            versions[0].require.get("php").map(String::as_str),
            Some(">=8.1")
        );
        let al = versions[0].autoload.as_ref().expect("autoload");
        assert_eq!(al.classmap, vec!["src/Foo.php"]);
        assert_eq!(al.psr4.get("Acme\\").unwrap().paths(), vec!["src/", "lib/"]);
    }

    #[test]
    fn metadata_identity_includes_origin() {
        let a = RepositoryClient::with_base_url("https://repo-a.example").unwrap();
        let b = RepositoryClient::with_base_url("https://repo-b.example").unwrap();
        assert_ne!(
            a.metadata_identity("acme/shared"),
            b.metadata_identity("acme/shared")
        );
        assert_ne!(a.disk_path("acme/shared"), b.disk_path("acme/shared"));
    }

    #[test]
    fn packagist_disabled_does_not_insert_default() {
        let manifest = composer_manifest::ComposerJson::from_str(
            r#"{"repositories":{"packagist.org":false}}"#,
        )
        .unwrap();
        let reg = RepositoryRegistry::from_manifest_auth(&manifest, AuthStore::default()).unwrap();
        assert_eq!(reg.repository_count(), 0);
    }

    #[test]
    fn secure_http_rejects_plaintext_composer_repo() {
        let manifest = composer_manifest::ComposerJson::from_str(
            r#"{"repositories":[{"type":"composer","url":"http://repo.example"}]}"#,
        )
        .unwrap();
        let err = match RepositoryRegistry::from_manifest_auth(&manifest, AuthStore::default()) {
            Ok(_) => panic!("expected insecure HTTP to be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("insecure"), "{err}");
    }

    #[test]
    fn secure_http_false_allows_plaintext_repo() {
        let manifest = composer_manifest::ComposerJson::from_str(
            r#"{
                "config": {"secure-http": false},
                "repositories": {
                    "custom": {"type":"composer","url":"http://repo.example"},
                    "packagist.org": false
                }
            }"#,
        )
        .unwrap();
        let reg = RepositoryRegistry::from_manifest_auth(&manifest, AuthStore::default()).unwrap();
        assert_eq!(reg.repository_count(), 1);
    }
}
