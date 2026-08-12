//! Packagist and Composer repository API client.

#![deny(unsafe_code)]

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
        })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Fetch all versions of a package (Composer API v2).
    pub async fn get_package_versions(&self, name: &PackageId) -> Result<Vec<RemotePackageVersion>> {
        let key = name.as_str().to_string();

        if let Some(cached) = self.memory.get(&key) {
            if cached.fetched_at.elapsed().unwrap_or(Duration::MAX) < self.metadata_ttl {
                return Ok(cached.versions.clone());
            }
        }

        // Disk cache
        if let Some(versions) = self.load_disk_cache(&key)? {
            self.memory.insert(
                key.clone(),
                CachedPackage {
                    versions: versions.clone(),
                    fetched_at: SystemTime::now(),
                },
            );
            return Ok(versions);
        }

        let url = format!("{}/p2/{}.json", self.base_url, key);
        debug!(%url, "fetching package metadata");

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::download(&url, e.to_string()))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::PackageNotFound(key));
        }

        if !resp.status().is_success() {
            return Err(Error::download(
                &url,
                format!("HTTP {}", resp.status()),
            ));
        }

        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::download(&url, e.to_string()))?;

        let versions = parse_p2_response(&key, &body)?;
        self.save_disk_cache(&key, &body)?;

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
            .filter(|v| v.version.stability() as u8 >= min as u8 || constraint.as_str().contains("dev"))
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

    /// Search packages via Packagist search API.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let url = format!(
            "https://packagist.org/search.json?q={}&per_page={}",
            urlencoding_minimal(query),
            limit
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::download(&url, e.to_string()))?;
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

    fn disk_path(&self, package: &str) -> PathBuf {
        let hash = blake3::hash(package.as_bytes());
        let hex = hash.to_hex();
        metadata_dir()
            .join("p2")
            .join(&hex.as_str()[..2])
            .join(format!("{package}.json").replace('/', "_"))
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
    /// Build from `composer.json` repositories (respects `packagist.org: false`).
    pub fn from_manifest(manifest: &composer_manifest::ComposerJson) -> Result<Self> {
        let repos = manifest.repositories_list();
        let mut clients = Vec::new();
        let mut has_packagist = false;

        for repo in &repos {
            if let composer_manifest::Repository::Composer { url } = repo {
                let normalized = url.trim_end_matches('/');
                if normalized.contains("packagist.org") {
                    has_packagist = true;
                }
                clients.push(RepositoryClient::with_base_url(url)?);
            }
        }

        if manifest.packagist_enabled() && !has_packagist {
            clients.push(RepositoryClient::new()?);
        }

        if clients.is_empty() {
            clients.push(RepositoryClient::new()?);
        }

        Ok(Self { clients })
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

    /// Delegate search to the first repository client.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        self.clients
            .first()
            .ok_or_else(|| Error::other("no repository configured"))?
            .search(query, limit)
            .await
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

#[derive(Debug, Deserialize)]
struct P2Response {
    packages: BTreeMap<String, Vec<P2Package>>,
}

/// Packagist p2 may emit `"__unset"` for inherited fields that were cleared.
fn deserialize_string_map<'de, D>(deserializer: D) -> std::result::Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Object(map) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    out.insert(k, s.to_string());
                }
            }
            Ok(out)
        }
        // "__unset" or null or other → empty
        _ => Ok(BTreeMap::new()),
    }
}

fn deserialize_optional_autoload<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<AutoloadConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Object(_) => serde_json::from_value(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None), // "__unset", null, etc.
    }
}

fn deserialize_optional_dist<'de, D>(deserializer: D) -> std::result::Result<Option<P2Dist>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Object(_) => serde_json::from_value(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

fn deserialize_optional_source<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<P2Source>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Object(_) => serde_json::from_value(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct P2Package {
    version: String,
    #[serde(default, rename = "version_normalized")]
    version_normalized: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_dist")]
    dist: Option<P2Dist>,
    #[serde(default, deserialize_with = "deserialize_optional_source")]
    source: Option<P2Source>,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    require: BTreeMap<String, String>,
    #[serde(
        default,
        rename = "require-dev",
        deserialize_with = "deserialize_string_map"
    )]
    require_dev: BTreeMap<String, String>,
    #[serde(default, rename = "type")]
    package_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_autoload")]
    autoload: Option<AutoloadConfig>,
    #[serde(
        default,
        rename = "autoload-dev",
        deserialize_with = "deserialize_optional_autoload"
    )]
    autoload_dev: Option<AutoloadConfig>,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    provide: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    replace: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    conflict: BTreeMap<String, String>,
    #[serde(default)]
    bin: serde_json::Value,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: serde_json::Value,
    #[serde(default)]
    abandoned: Option<serde_json::Value>,
    #[serde(default)]
    time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct P2Dist {
    #[serde(rename = "type")]
    dist_type: String,
    url: String,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    shasum: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct P2Source {
    #[serde(rename = "type")]
    source_type: String,
    url: String,
    #[serde(default)]
    reference: Option<String>,
}

fn parse_p2_response(package: &str, body: &[u8]) -> Result<Vec<RemotePackageVersion>> {
    let parsed: P2Response =
        serde_json::from_slice(body).map_err(|e| Error::other(format!("p2 parse: {e}")))?;

    let list = parsed
        .packages
        .get(package)
        .or_else(|| parsed.packages.values().next())
        .cloned()
        .unwrap_or_default();

    let mut versions = Vec::with_capacity(list.len());
    for p in list {
        let version = match ComposerVersion::parse(&p.version) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let bin = match p.bin {
            serde_json::Value::String(s) => vec![s],
            serde_json::Value::Array(arr) => arr
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => vec![],
        };
        let license = match p.license {
            serde_json::Value::String(s) => vec![s],
            serde_json::Value::Array(arr) => arr
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => vec![],
        };

        versions.push(RemotePackageVersion {
            name: package.to_string(),
            version,
            version_normalized: p.version_normalized.unwrap_or_default(),
            dist: p.dist.map(|d| DistInfo {
                dist_type: d.dist_type,
                url: d.url,
                reference: d.reference,
                shasum: d.shasum,
                mirrors: None,
            }),
            source: p.source.map(|s| SourceInfo {
                source_type: s.source_type,
                url: s.url,
                reference: s.reference,
            }),
            require: p.require,
            require_dev: p.require_dev,
            package_type: p.package_type,
            autoload: p.autoload,
            autoload_dev: p.autoload_dev,
            provide: p.provide,
            replace: p.replace,
            conflict: p.conflict,
            bin,
            description: p.description,
            license,
            abandoned: p.abandoned,
            time: p.time,
        });
    }

    Ok(versions)
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
}
