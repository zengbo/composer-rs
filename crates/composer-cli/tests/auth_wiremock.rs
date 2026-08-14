//! Integration: HTTP Basic auth is applied to repository metadata fetches.

use composer_auth::{AuthStore, HttpBasic};
use composer_core::PackageId;
use composer_manifest::ComposerJson;
use composer_repo::RepositoryRegistry;
use std::sync::Mutex;
use wiremock::matchers::{basic_auth, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Isolate metadata disk cache (same issue as resolver mock tests).
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

fn host_from_uri(uri: &str) -> String {
    let u = url::Url::parse(uri).unwrap();
    match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap()),
        None => u.host_str().unwrap().to_string(),
    }
}

#[tokio::test]
async fn p2_fetch_sends_http_basic_credentials() {
    let _cache = CacheEnvGuard::new();
    let server = MockServer::start().await;

    let p2 = r#"{
        "packages": {
            "acme/secret": [
                {
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "dist": {
                        "type": "zip",
                        "url": "https://example.com/secret.zip",
                        "reference": "x"
                    },
                    "require": { "php": ">=8.0" },
                    "type": "library"
                }
            ]
        }
    }"#;

    Mock::given(method("GET"))
        .and(path("/p2/acme/secret.json"))
        .and(basic_auth("ci-user", "s3cret"))
        .respond_with(ResponseTemplate::new(200).set_body_string(p2))
        .expect(1)
        .mount(&server)
        .await;

    let mut auth = AuthStore::default();
    auth.http_basic.insert(
        host_from_uri(&server.uri()),
        HttpBasic {
            username: "ci-user".into(),
            password: "s3cret".into(),
        },
    );

    let manifest_json = format!(
        r#"{{
            "name": "acme/app",
            "require": {{ "php": ">=8.0", "acme/secret": "^1.0" }},
            "config": {{ "secure-http": false }},
            "repositories": {{
                "private": {{ "type": "composer", "url": "{}/" }},
                "packagist.org": false
            }}
        }}"#,
        server.uri()
    );
    let manifest = ComposerJson::from_str(&manifest_json).unwrap();
    let registry = RepositoryRegistry::from_manifest_auth(&manifest, auth).unwrap();
    let id = PackageId::parse("acme/secret").unwrap();
    let versions = registry
        .get_package_versions(&id)
        .await
        .expect("authenticated p2 fetch");
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].version.raw, "1.0.0");
}

#[tokio::test]
async fn search_sends_http_basic_credentials() {
    let _cache = CacheEnvGuard::new();
    let server = MockServer::start().await;

    let body = r#"{
        "results": [
            {
                "name": "acme/secret",
                "description": "private",
                "url": "https://example.com/acme/secret",
                "repository": "https://example.com/acme/secret",
                "downloads": 1
            }
        ],
        "total": 1
    }"#;

    Mock::given(method("GET"))
        .and(path("/search.json"))
        .and(basic_auth("search-user", "search-pass"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/search.json"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let mut auth = AuthStore::default();
    auth.http_basic.insert(
        host_from_uri(&server.uri()),
        HttpBasic {
            username: "search-user".into(),
            password: "search-pass".into(),
        },
    );

    let client = composer_repo::RepositoryClient::with_base_url_auth(server.uri(), auth)
        .unwrap()
        .with_secure_http(false);
    let results = client.search("secret", 10).await.expect("search with auth");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "acme/secret");
}

#[tokio::test]
async fn p2_fetch_without_auth_fails_on_protected_repo() {
    let _cache = CacheEnvGuard::new();
    let server = MockServer::start().await;

    // Only the authorized matcher exists — no anonymous 200.
    Mock::given(method("GET"))
        .and(path("/p2/acme/locked.json"))
        .and(basic_auth("ci-user", "s3cret"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"packages":{"acme/locked":[{"version":"1.0.0","dist":{"type":"zip","url":"https://x","reference":"a"},"type":"library"}]}}"#,
        ))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/p2/acme/locked.json"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let manifest_json = format!(
        r#"{{
            "name": "acme/app",
            "require": {{ "acme/locked": "*" }},
            "config": {{ "secure-http": false }},
            "repositories": {{
                "private": {{ "type": "composer", "url": "{}/" }},
                "packagist.org": false
            }}
        }}"#,
        server.uri()
    );
    let manifest = ComposerJson::from_str(&manifest_json).unwrap();
    let registry = RepositoryRegistry::from_manifest_auth(&manifest, AuthStore::default()).unwrap();
    let id = PackageId::parse("acme/locked").unwrap();
    let err = registry.get_package_versions(&id).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("401") || msg.contains("download") || msg.contains("HTTP"),
        "expected auth failure, got: {msg}"
    );
}
