//! Integration: outdated Current/Wanted/Latest against a mock Composer v2 repo.

use composer_auth::AuthStore;
use composer_cli::commands::outdated::compute_row;
use composer_core::PackageId;
use composer_lock::{ComposerLock, DistInfo, LockedPackage};
use composer_manifest::ComposerJson;
use composer_repo::RepositoryRegistry;
use std::collections::BTreeMap;
use std::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());

struct CacheEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    _dir: tempfile::TempDir,
}

impl CacheEnvGuard {
    fn new() -> Self {
        let lock = CACHE_ENV_LOCK.lock().expect("cache env lock");
        let dir = tempfile::tempdir().expect("cache tempdir");
        let previous = std::env::var_os("COMPOSER_RS_CACHE");
        unsafe {
            std::env::set_var("COMPOSER_RS_CACHE", dir.path());
        }
        Self {
            _lock: lock,
            previous,
            _dir: dir,
        }
    }
}

impl Drop for CacheEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(v) => std::env::set_var("COMPOSER_RS_CACHE", v),
                None => std::env::remove_var("COMPOSER_RS_CACHE"),
            }
        }
    }
}

fn locked(name: &str, ver: &str) -> LockedPackage {
    LockedPackage {
        name: name.into(),
        version: ver.into(),
        source: None,
        dist: Some(DistInfo {
            dist_type: "zip".into(),
            url: format!("https://example.com/{name}-{ver}.zip"),
            reference: Some("ref".into()),
            shasum: None,
            mirrors: None,
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

#[tokio::test]
async fn outdated_wanted_and_latest_from_mock_p2() {
    let _cache = CacheEnvGuard::new();
    let server = MockServer::start().await;
    let p2 = r#"{
        "packages": {
            "acme/foo": [
                {
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "dist": { "type": "zip", "url": "https://example.com/1.zip", "reference": "a" },
                    "require": { "php": ">=8.0" },
                    "type": "library"
                },
                {
                    "version": "1.5.0",
                    "version_normalized": "1.5.0.0",
                    "dist": { "type": "zip", "url": "https://example.com/1.5.zip", "reference": "b" },
                    "require": { "php": ">=8.0" },
                    "type": "library"
                },
                {
                    "version": "2.0.0",
                    "version_normalized": "2.0.0.0",
                    "dist": { "type": "zip", "url": "https://example.com/2.zip", "reference": "c" },
                    "require": { "php": ">=8.0" },
                    "type": "library"
                }
            ]
        }
    }"#;

    Mock::given(method("GET"))
        .and(path("/p2/acme/foo.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(p2))
        .mount(&server)
        .await;

    let manifest_json = format!(
        r#"{{
            "name": "acme/app",
            "require": {{ "php": ">=8.0", "acme/foo": "^1.0" }},
            "repositories": {{
                "mock": {{ "type": "composer", "url": "{}/" }},
                "packagist.org": false
            }}
        }}"#,
        server.uri()
    );
    let manifest = ComposerJson::from_str(&manifest_json).unwrap();
    let lock = ComposerLock {
        packages: vec![locked("acme/foo", "1.0.0")],
        ..Default::default()
    };

    let registry = RepositoryRegistry::from_manifest_auth(&manifest, AuthStore::default()).unwrap();
    let id = PackageId::parse("acme/foo").unwrap();
    let versions = registry.get_package_versions(&id).await.expect("fetch p2");

    let pkg = lock.find("acme/foo").unwrap();
    let row = compute_row(pkg, &versions, Some("^1.0"));
    assert_eq!(row.current, "1.0.0");
    assert_eq!(row.wanted, "1.5.0");
    assert_eq!(row.latest, "2.0.0");
    assert!(row.is_outdated());
    assert_eq!(
        row.status,
        composer_cli::commands::outdated::OutdatedKind::SemverSafe
    );
    assert_eq!(row.status.marker(), '!');
}

#[tokio::test]
async fn outdated_major_marker_when_at_constraint_ceiling() {
    let _cache = CacheEnvGuard::new();
    let server = MockServer::start().await;
    let p2 = r#"{
        "packages": {
            "acme/bar": [
                {
                    "version": "1.9.0",
                    "version_normalized": "1.9.0.0",
                    "dist": { "type": "zip", "url": "https://example.com/1.9.zip", "reference": "a" },
                    "require": { "php": ">=8.0" },
                    "type": "library"
                },
                {
                    "version": "2.0.0",
                    "version_normalized": "2.0.0.0",
                    "dist": { "type": "zip", "url": "https://example.com/2.zip", "reference": "b" },
                    "require": { "php": ">=8.0" },
                    "type": "library"
                }
            ]
        }
    }"#;
    Mock::given(method("GET"))
        .and(path("/p2/acme/bar.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(p2))
        .mount(&server)
        .await;

    let manifest_json = format!(
        r#"{{
            "name": "acme/app",
            "require": {{ "php": ">=8.0", "acme/bar": "^1.0" }},
            "repositories": {{
                "mock": {{ "type": "composer", "url": "{}/" }},
                "packagist.org": false
            }}
        }}"#,
        server.uri()
    );
    let manifest = ComposerJson::from_str(&manifest_json).unwrap();
    let lock = ComposerLock {
        packages: vec![locked("acme/bar", "1.9.0")],
        ..Default::default()
    };
    let registry = RepositoryRegistry::from_manifest_auth(&manifest, AuthStore::default()).unwrap();
    let id = PackageId::parse("acme/bar").unwrap();
    let versions = registry.get_package_versions(&id).await.unwrap();
    let row = compute_row(lock.find("acme/bar").unwrap(), &versions, Some("^1.0"));
    assert_eq!(row.wanted, "1.9.0");
    assert_eq!(row.latest, "2.0.0");
    assert_eq!(
        row.status,
        composer_cli::commands::outdated::OutdatedKind::MajorOnly
    );
    assert_eq!(row.status.marker(), '~');
}
