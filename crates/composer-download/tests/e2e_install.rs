//! End-to-end: resolve → install → vendor/ (+ autoload).
//! Offline only: path repos and wiremock-served dist zips.

use composer_autoload::{AutoloadOptions, generate};
use composer_cache::CasCache;
use composer_download::PackageInstaller;
use composer_lock::{ComposerLock, DistInfo, LockedPackage};
use composer_manifest::ComposerJson;
use composer_resolver::{ResolveOptions, resolve};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize mutations of `COMPOSER_RS_CACHE` without holding `std::sync` locks across await.
static CACHE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Isolate archive + metadata cache for a single test (restored on drop).
struct CacheEnvGuard {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    dir: tempfile::TempDir,
}

impl CacheEnvGuard {
    async fn new() -> Self {
        let lock = CACHE_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("cache tempdir");
        let previous = std::env::var_os("COMPOSER_RS_CACHE");
        // SAFETY: exclusive via CACHE_ENV_LOCK; restored in Drop.
        unsafe {
            std::env::set_var("COMPOSER_RS_CACHE", dir.path());
        }
        Self {
            _lock: lock,
            previous,
            dir,
        }
    }

    fn cas_root(&self) -> PathBuf {
        self.dir.path().join("cas")
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

fn write_file(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn resolve_opts() -> ResolveOptions {
    ResolveOptions {
        with_dev: false,
        prefer_stable: true,
        prefer_lowest: false,
        minimum_stability: "stable".into(),
        concurrency: 4,
        ignore_platform_reqs: false,
        ignore_platform_req: Vec::new(),
        packages_to_update: Vec::new(),
        update_deps: Default::default(),
    }
}

async fn resolve_and_install(app_dir: &Path, cache_dir: &Path) -> (ComposerLock, PathBuf) {
    let manifest_path = app_dir.join("composer.json");
    let manifest = ComposerJson::load(&manifest_path).expect("load composer.json");
    let options = resolve_opts();

    let resolution = resolve(&manifest, &options, app_dir, None)
        .await
        .expect("resolve");
    let lock = resolution.to_lock(&manifest);

    let vendor = app_dir.join("vendor");
    fs::create_dir_all(&vendor).unwrap();

    let installer = PackageInstaller::new(4, false)
        .expect("installer")
        .with_project_root(app_dir)
        .with_cache(CasCache::with_root(cache_dir));

    let packages = composer_resolver::locked_list(&lock, false);
    let refs: Vec<_> = packages.iter().collect();
    installer
        .install_all(&refs, &vendor)
        .await
        .expect("install_all");

    generate(
        app_dir,
        &vendor,
        &manifest,
        Some(&lock),
        &AutoloadOptions {
            optimize: false,
            classmap_authoritative: false,
            with_dev: false,
        },
    )
    .expect("autoload");

    (lock, vendor)
}

#[tokio::test]
async fn e2e_path_repo_resolve_install_autoload() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let lib = root.join("packages/acme-lib");
    write_file(
        &lib.join("composer.json"),
        r#"{
            "name": "acme/lib",
            "version": "1.0.0",
            "type": "library",
            "autoload": {
                "psr-4": { "Acme\\Lib\\": "src/" }
            }
        }"#,
    );
    write_file(
        &lib.join("src/Hello.php"),
        "<?php\nnamespace Acme\\Lib;\nclass Hello { public static function hi() { return 'ok'; } }\n",
    );

    let app = root.join("app");
    write_file(
        &app.join("composer.json"),
        r#"{
            "name": "acme/app",
            "require": {
                "php": ">=8.0",
                "acme/lib": "*"
            },
            "repositories": [
                { "type": "path", "url": "../packages/acme-lib" }
            ],
            "config": {
                "platform": { "php": "8.2.0" }
            }
        }"#,
    );

    let cache = root.join("cache");
    let (lock, vendor) = resolve_and_install(&app, &cache).await;

    assert_eq!(lock.packages.len(), 1);
    assert_eq!(lock.packages[0].name, "acme/lib");
    assert_eq!(lock.packages[0].version, "1.0.0");

    let installed = vendor.join("acme/lib");
    assert!(
        installed.exists(),
        "expected vendor/acme/lib to exist after path install"
    );
    assert!(
        installed.join("src/Hello.php").is_file()
            || fs::read_link(&installed).is_ok_and(|t| t.join("src/Hello.php").is_file()),
        "Hello.php missing under installed path package"
    );

    assert!(vendor.join("autoload.php").is_file());
    assert!(vendor.join("composer/autoload_psr4.php").is_file());
    let psr4 = fs::read_to_string(vendor.join("composer/autoload_psr4.php")).unwrap();
    assert!(
        psr4.contains("Acme\\\\Lib\\\\") || psr4.contains("Acme\\Lib\\"),
        "autoload_psr4 should map Acme\\Lib\\:\n{psr4}"
    );
}

fn make_zip_with_files(files: &[(&str, &str)]) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[tokio::test]
async fn e2e_dist_zip_resolve_install() {
    let cache_env = CacheEnvGuard::new().await;
    let server = MockServer::start().await;
    let zip_bytes = make_zip_with_files(&[
        (
            "acme-foo-1.0.0/composer.json",
            r#"{"name":"acme/foo","version":"1.0.0","autoload":{"psr-4":{"Acme\\Foo\\":"src/"}}}"#,
        ),
        (
            "acme-foo-1.0.0/src/Widget.php",
            "<?php\nnamespace Acme\\Foo;\nclass Widget {}\n",
        ),
    ]);

    Mock::given(method("GET"))
        .and(path("/dist/acme-foo-1.0.0.zip"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(zip_bytes),
        )
        .mount(&server)
        .await;

    let p2 = format!(
        r#"{{
            "packages": {{
                "acme/foo": [{{
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "dist": {{
                        "type": "zip",
                        "url": "{base}/dist/acme-foo-1.0.0.zip",
                        "reference": "abc123"
                    }},
                    "require": {{ "php": ">=8.0" }},
                    "type": "library",
                    "autoload": {{
                        "psr-4": {{ "Acme\\Foo\\": "src/" }}
                    }}
                }}]
            }}
        }}"#,
        base = server.uri()
    );

    Mock::given(method("GET"))
        .and(path("/p2/acme/foo.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(p2))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();

    let manifest_json = format!(
        r#"{{
            "name": "acme/app",
            "require": {{
                "php": ">=8.0",
                "acme/foo": "^1.0"
            }},
            "repositories": {{
                "mock": {{ "type": "composer", "url": "{}/" }},
                "packagist.org": false
            }},
            "config": {{
                "platform": {{ "php": "8.2.0" }}
            }}
        }}"#,
        server.uri()
    );
    write_file(&app.join("composer.json"), &manifest_json);

    let (lock, vendor) = resolve_and_install(&app, &cache_env.cas_root()).await;

    assert_eq!(lock.packages.len(), 1);
    assert_eq!(lock.packages[0].name, "acme/foo");
    assert_eq!(lock.packages[0].version, "1.0.0");

    let widget = vendor.join("acme/foo/src/Widget.php");
    assert!(
        widget.is_file(),
        "expected dist extract at {}",
        widget.display()
    );
    let body = fs::read_to_string(&widget).unwrap();
    assert!(body.contains("class Widget"));

    assert!(vendor.join("autoload.php").is_file());
}

fn locked_dist(name: &str, url: &str, mirrors: Option<Vec<serde_json::Value>>) -> LockedPackage {
    LockedPackage {
        name: name.into(),
        version: "1.0.0".into(),
        source: None,
        dist: Some(DistInfo {
            dist_type: "zip".into(),
            url: url.into(),
            reference: Some("mirror-ref".into()),
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

fn zip_lib(name: &str, class: &str) -> Vec<u8> {
    let folder = format!("{}-1.0.0", name.replace('/', "-"));
    let composer_path = format!("{folder}/composer.json");
    let composer_body = format!(r#"{{"name":"{name}","version":"1.0.0"}}"#);
    let php_path = format!("{folder}/src/{class}.php");
    let php_body = format!("<?php\nclass {class} {{}}\n");
    make_zip_with_files(&[
        (composer_path.as_str(), composer_body.as_str()),
        (php_path.as_str(), php_body.as_str()),
    ])
}

async fn install_one_pkg(
    app: &Path,
    cache_dir: &Path,
    pkg: &LockedPackage,
) -> composer_core::error::Result<()> {
    let vendor = app.join("vendor");
    fs::create_dir_all(&vendor).unwrap();
    let installer = PackageInstaller::new(2, false)
        .expect("installer")
        .with_project_root(app)
        .with_cache(CasCache::with_root(cache_dir));
    installer.install_all(&[pkg], &vendor).await
}

#[tokio::test]
async fn e2e_dist_mirror_failover_when_primary_fails() {
    let cache_env = CacheEnvGuard::new().await;
    let server = MockServer::start().await;
    let zip_bytes = zip_lib("acme/mirror-lib", "MirrorHit");

    Mock::given(method("GET"))
        .and(path("/dist/primary.zip"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/dist/mirror.zip"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(zip_bytes),
        )
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();

    let pkg = locked_dist(
        "acme/mirror-lib",
        &format!("{}/dist/primary.zip", server.uri()),
        Some(vec![
            serde_json::json!(format!("{}/dist/mirror.zip", server.uri())),
            serde_json::json!({"url": format!("{}/dist/unused.zip", server.uri())}),
        ]),
    );

    install_one_pkg(&app, &cache_env.cas_root(), &pkg)
        .await
        .expect("install should fail over to mirror");

    let hit = app.join("vendor/acme/mirror-lib/src/MirrorHit.php");
    assert!(
        hit.is_file(),
        "expected extract from mirror at {}",
        hit.display()
    );
}

#[tokio::test]
async fn e2e_dist_mirrors_all_fail() {
    let cache_env = CacheEnvGuard::new().await;
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dist/primary.zip"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dist/mirror.zip"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();

    let pkg = locked_dist(
        "acme/mirror-miss",
        &format!("{}/dist/primary.zip", server.uri()),
        Some(vec![serde_json::json!(format!(
            "{}/dist/mirror.zip",
            server.uri()
        ))]),
    );

    let err = install_one_pkg(&app, &cache_env.cas_root(), &pkg)
        .await
        .expect_err("install must fail when every dist URL fails");
    let msg = err.to_string();
    assert!(
        msg.contains("failed to install") || msg.contains("HTTP"),
        "unexpected error: {msg}"
    );
    assert!(
        !app.join("vendor/acme/mirror-miss/composer.json").is_file(),
        "failed install must not leave a package tree"
    );
}

#[tokio::test]
async fn e2e_resolve_preserves_p2_mirrors_and_fails_over() {
    let cache_env = CacheEnvGuard::new().await;
    let server = MockServer::start().await;
    let zip_bytes = zip_lib("acme/p2-mirror", "FromP2Mirror");

    Mock::given(method("GET"))
        .and(path("/dist/p2-primary.zip"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/dist/p2-mirror.zip"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(zip_bytes),
        )
        .expect(1)
        .mount(&server)
        .await;

    let p2 = format!(
        r#"{{
            "packages": {{
                "acme/p2-mirror": [{{
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "dist": {{
                        "type": "zip",
                        "url": "{base}/dist/p2-primary.zip",
                        "reference": "p2ref",
                        "mirrors": [
                            "{base}/dist/p2-mirror.zip",
                            {{ "url": "{base}/dist/p2-unused.zip" }}
                        ]
                    }},
                    "require": {{ "php": ">=8.0" }},
                    "type": "library"
                }}]
            }}
        }}"#,
        base = server.uri()
    );

    Mock::given(method("GET"))
        .and(path("/p2/acme/p2-mirror.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(p2))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    write_file(
        &app.join("composer.json"),
        &format!(
            r#"{{
                "name": "acme/app",
                "require": {{
                    "php": ">=8.0",
                    "acme/p2-mirror": "^1.0"
                }},
                "repositories": {{
                    "mock": {{ "type": "composer", "url": "{}/" }},
                    "packagist.org": false
                }},
                "config": {{
                    "platform": {{ "php": "8.2.0" }}
                }}
            }}"#,
            server.uri()
        ),
    );

    let (lock, vendor) = resolve_and_install(&app, &cache_env.cas_root()).await;

    assert_eq!(lock.packages.len(), 1);
    let dist = lock.packages[0].dist.as_ref().expect("dist on lock");
    let mirrors = dist
        .mirrors
        .as_ref()
        .expect("p2 mirrors must survive resolve");
    assert!(
        mirrors.iter().any(|m| m
            .as_str()
            .is_some_and(|u| u.ends_with("/dist/p2-mirror.zip"))),
        "lock should list string mirror: {mirrors:?}"
    );

    let hit = vendor.join("acme/p2-mirror/src/FromP2Mirror.php");
    assert!(
        hit.is_file(),
        "resolve+install should download the p2 mirror after primary 503"
    );
}
