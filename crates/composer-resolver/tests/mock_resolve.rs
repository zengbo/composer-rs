//! Integration tests: resolve against a mock Packagist-compatible HTTP server.
//! Covers full resolve and partial-update pinning (`packages_to_update` + `-w`/`-W`).

use composer_lock::{ComposerLock, DistInfo, LockedPackage};
use composer_manifest::ComposerJson;
use composer_resolver::{resolve, ResolveOptions, UpdateDeps};
use std::collections::BTreeMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize mutations of `COMPOSER_RS_CACHE` without holding `std::sync` locks across await.
static CACHE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Isolate Packagist metadata cache so mock responses are not reused across tests.
struct CacheEnvGuard {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    _dir: tempfile::TempDir,
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

fn opts(partial: &[&str], update_deps: UpdateDeps) -> ResolveOptions {
    ResolveOptions {
        with_dev: true,
        prefer_stable: true,
        prefer_lowest: false,
        minimum_stability: "stable".into(),
        concurrency: 4,
        ignore_platform_reqs: false,
        packages_to_update: partial.iter().map(|s| (*s).to_string()).collect(),
        update_deps,
    }
}

fn locked(name: &str, ver: &str, require: &[(&str, &str)]) -> LockedPackage {
    locked_full(name, ver, require, &[])
}

fn locked_full(
    name: &str,
    ver: &str,
    require: &[(&str, &str)],
    require_dev: &[(&str, &str)],
) -> LockedPackage {
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
        require: require
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
        require_dev: require_dev
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
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

fn p2_versions(name: &str, versions: &[(&str, &str)]) -> String {
    // versions: (version, require_json_object_body) e.g. (`"1.0.0"`, `{"php":">=8.0"}`)
    let entries: Vec<String> = versions
        .iter()
        .map(|(ver, require_body)| {
            format!(
                r#"{{
                    "version": "{ver}",
                    "version_normalized": "{ver}.0",
                    "dist": {{
                        "type": "zip",
                        "url": "https://example.com/{name}-{ver}.zip",
                        "reference": "{ver}"
                    }},
                    "require": {require_body},
                    "type": "library"
                }}"#
            )
        })
        .collect();
    format!(
        r#"{{ "packages": {{ "{name}": [ {list} ] }} }}"#,
        list = entries.join(",\n")
    )
}

async fn mount_p2(server: &MockServer, package: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/p2/{package}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
        .mount(server)
        .await;
}

fn app_manifest(server: &str, require: &str) -> String {
    format!(
        r#"{{
            "name": "acme/app",
            "require": {{
                "php": ">=8.1",
                {require}
            }},
            "repositories": {{
                "mock": {{ "type": "composer", "url": "{server}/" }},
                "packagist.org": false
            }},
            "config": {{
                "platform": {{ "php": "8.2.0" }}
            }}
        }}"#
    )
}

fn app_manifest_require_dev(server: &str, require_dev: &str) -> String {
    format!(
        r#"{{
            "name": "acme/app",
            "require": {{
                "php": ">=8.1"
            }},
            "require-dev": {{
                {require_dev}
            }},
            "repositories": {{
                "mock": {{ "type": "composer", "url": "{server}/" }},
                "packagist.org": false
            }},
            "config": {{
                "platform": {{ "php": "8.2.0" }}
            }}
        }}"#
    )
}

#[tokio::test]
async fn resolve_package_from_mock_repository() {
    let _cache = CacheEnvGuard::new().await;
    let server = MockServer::start().await;
    mount_p2(
        &server,
        "acme/foo",
        &p2_versions(
            "acme/foo",
            &[
                ("1.0.0", r#"{ "php": ">=8.1" }"#),
                ("2.0.0", r#"{ "php": ">=8.1" }"#),
            ],
        ),
    )
    .await;

    let manifest_json = app_manifest(&server.uri(), r#""acme/foo": "^2.0""#);
    let manifest = ComposerJson::from_str(&manifest_json).expect("manifest parses");
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("composer.json"), &manifest_json).unwrap();

    let resolution = resolve(&manifest, &opts(&[], UpdateDeps::OnlyListed), tmp.path(), None)
        .await
        .expect("resolve succeeds");

    assert_eq!(resolution.packages.len(), 1);
    assert_eq!(resolution.packages[0].name, "acme/foo");
    assert_eq!(resolution.packages[0].version, "2.0.0");
}

/// Partial update of only `acme/foo` must leave `acme/bar` at the locked version
/// even when a newer bar is available.
#[tokio::test]
async fn partial_update_pins_unlisted_package() {
    let _cache = CacheEnvGuard::new().await;
    let server = MockServer::start().await;
    mount_p2(
        &server,
        "acme/foo",
        &p2_versions(
            "acme/foo",
            &[
                ("1.0.0", r#"{ "php": ">=8.1" }"#),
                ("2.0.0", r#"{ "php": ">=8.1" }"#),
            ],
        ),
    )
    .await;
    mount_p2(
        &server,
        "acme/bar",
        &p2_versions(
            "acme/bar",
            &[
                ("1.0.0", r#"{ "php": ">=8.1" }"#),
                ("2.0.0", r#"{ "php": ">=8.1" }"#),
            ],
        ),
    )
    .await;

    let manifest_json = app_manifest(
        &server.uri(),
        r#""acme/foo": "^1.0 || ^2.0", "acme/bar": "^1.0 || ^2.0""#,
    );
    let manifest = ComposerJson::from_str(&manifest_json).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("composer.json"), &manifest_json).unwrap();

    let existing = ComposerLock {
        packages: vec![
            locked("acme/foo", "1.0.0", &[]),
            locked("acme/bar", "1.0.0", &[]),
        ],
        ..Default::default()
    };

    let resolution = resolve(
        &manifest,
        &opts(&["acme/foo"], UpdateDeps::OnlyListed),
        tmp.path(),
        Some(&existing),
    )
    .await
    .expect("partial resolve");

    let by_name: BTreeMap<_, _> = resolution
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    assert_eq!(by_name.get("acme/foo"), Some(&"2.0.0"), "foo should update");
    assert_eq!(
        by_name.get("acme/bar"),
        Some(&"1.0.0"),
        "bar must stay pinned at lock"
    );
}

/// `-w` frees non-root transitive deps of the listed package so they can update.
#[tokio::test]
async fn partial_update_with_w_frees_transitive_dep() {
    let _cache = CacheEnvGuard::new().await;
    let server = MockServer::start().await;
    // foo@1 requires util@^1; foo@2 requires util@^2
    mount_p2(
        &server,
        "acme/foo",
        &p2_versions(
            "acme/foo",
            &[
                ("1.0.0", r#"{ "php": ">=8.1", "acme/util": "^1.0" }"#),
                ("2.0.0", r#"{ "php": ">=8.1", "acme/util": "^2.0" }"#),
            ],
        ),
    )
    .await;
    mount_p2(
        &server,
        "acme/util",
        &p2_versions(
            "acme/util",
            &[
                ("1.0.0", r#"{ "php": ">=8.1" }"#),
                ("2.0.0", r#"{ "php": ">=8.1" }"#),
            ],
        ),
    )
    .await;

    // Only foo is a root requirement; util is transitive.
    let manifest_json = app_manifest(&server.uri(), r#""acme/foo": "^1.0 || ^2.0""#);
    let manifest = ComposerJson::from_str(&manifest_json).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("composer.json"), &manifest_json).unwrap();

    let existing = ComposerLock {
        packages: vec![
            locked("acme/foo", "1.0.0", &[("acme/util", "^1.0")]),
            locked("acme/util", "1.0.0", &[]),
        ],
        ..Default::default()
    };

    // Without -w: util pinned at 1.0 → cannot select foo 2.0 → stays on 1.x tree
    let only_listed = resolve(
        &manifest,
        &opts(&["acme/foo"], UpdateDeps::OnlyListed),
        tmp.path(),
        Some(&existing),
    )
    .await
    .expect("only-listed resolve");
    let only: BTreeMap<_, _> = only_listed
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    assert_eq!(
        only.get("acme/foo"),
        Some(&"1.0.0"),
        "without -w, foo cannot jump to 2.x while util is pinned"
    );
    assert_eq!(only.get("acme/util"), Some(&"1.0.0"));

    // With -w: util is free → foo 2.0 + util 2.0
    let with_w = resolve(
        &manifest,
        &opts(&["acme/foo"], UpdateDeps::WithDependencies),
        tmp.path(),
        Some(&existing),
    )
    .await
    .expect("-w resolve");
    let with: BTreeMap<_, _> = with_w
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    assert_eq!(with.get("acme/foo"), Some(&"2.0.0"));
    assert_eq!(with.get("acme/util"), Some(&"2.0.0"));
}

/// `-w` must not free a dependency that is also a root requirement; `-W` may.
#[tokio::test]
async fn partial_update_with_w_skips_root_req_but_with_all_frees_it() {
    let _cache = CacheEnvGuard::new().await;
    let server = MockServer::start().await;
    mount_p2(
        &server,
        "acme/foo",
        &p2_versions(
            "acme/foo",
            &[
                ("1.0.0", r#"{ "php": ">=8.1", "acme/bar": "^1.0" }"#),
                ("2.0.0", r#"{ "php": ">=8.1", "acme/bar": "^2.0" }"#),
            ],
        ),
    )
    .await;
    mount_p2(
        &server,
        "acme/bar",
        &p2_versions(
            "acme/bar",
            &[
                ("1.0.0", r#"{ "php": ">=8.1" }"#),
                ("2.0.0", r#"{ "php": ">=8.1" }"#),
            ],
        ),
    )
    .await;

    // Both are root requirements.
    let manifest_json = app_manifest(
        &server.uri(),
        r#""acme/foo": "^1.0 || ^2.0", "acme/bar": "^1.0 || ^2.0""#,
    );
    let manifest = ComposerJson::from_str(&manifest_json).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("composer.json"), &manifest_json).unwrap();

    let existing = ComposerLock {
        packages: vec![
            locked("acme/foo", "1.0.0", &[("acme/bar", "^1.0")]),
            locked("acme/bar", "1.0.0", &[]),
        ],
        ..Default::default()
    };

    let with_w = resolve(
        &manifest,
        &opts(&["acme/foo"], UpdateDeps::WithDependencies),
        tmp.path(),
        Some(&existing),
    )
    .await
    .expect("-w resolve");
    let w: BTreeMap<_, _> = with_w
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    // bar is root req → still pinned; foo cannot take 2.x
    assert_eq!(w.get("acme/bar"), Some(&"1.0.0"));
    assert_eq!(w.get("acme/foo"), Some(&"1.0.0"));

    let with_all = resolve(
        &manifest,
        &opts(&["acme/foo"], UpdateDeps::WithAllDependencies),
        tmp.path(),
        Some(&existing),
    )
    .await
    .expect("-W resolve");
    let wall: BTreeMap<_, _> = with_all
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    assert_eq!(wall.get("acme/foo"), Some(&"2.0.0"));
    assert_eq!(wall.get("acme/bar"), Some(&"2.0.0"));
}

/// Dev-tree partial update: `require-dev` edges are walked when `with_dev` is on.
///
/// Lock stores util only under phpunit's `require-dev` (not `require`). Without
/// walking `require-dev`, `-W` would not free util and phpunit could not reach 2.x.
#[tokio::test]
async fn partial_update_with_w_walks_require_dev_in_dev_tree() {
    let _cache = CacheEnvGuard::new().await;
    let server = MockServer::start().await;

    // phpunit@1: no prod require on util (only require-dev in lock).
    // phpunit@2: prod-requires util ^2 so the solver must select util 2.x.
    mount_p2(
        &server,
        "acme/phpunit",
        &p2_versions(
            "acme/phpunit",
            &[
                ("1.0.0", r#"{ "php": ">=8.1" }"#),
                (
                    "2.0.0",
                    r#"{ "php": ">=8.1", "acme/phpunit-util": "^2.0" }"#,
                ),
            ],
        ),
    )
    .await;
    mount_p2(
        &server,
        "acme/phpunit-util",
        &p2_versions(
            "acme/phpunit-util",
            &[
                ("1.0.0", r#"{ "php": ">=8.1" }"#),
                ("2.0.0", r#"{ "php": ">=8.1" }"#),
            ],
        ),
    )
    .await;

    // Both are root require-dev (so they appear in packages-dev).
    let manifest_json = app_manifest_require_dev(
        &server.uri(),
        r#""acme/phpunit": "^1.0 || ^2.0", "acme/phpunit-util": "^1.0 || ^2.0""#,
    );
    let manifest = ComposerJson::from_str(&manifest_json).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("composer.json"), &manifest_json).unwrap();

    let existing = ComposerLock {
        packages: vec![],
        packages_dev: vec![
            // util is only linked via require-dev on the locked phpunit snapshot
            locked_full(
                "acme/phpunit",
                "1.0.0",
                &[],
                &[("acme/phpunit-util", "^1.0")],
            ),
            locked("acme/phpunit-util", "1.0.0", &[]),
        ],
        ..Default::default()
    };

    // -w: util is a root require-dev → still pinned → phpunit cannot take 2.x
    let with_w = resolve(
        &manifest,
        &opts(&["acme/phpunit"], UpdateDeps::WithDependencies),
        tmp.path(),
        Some(&existing),
    )
    .await
    .expect("-w dev-tree resolve");
    let w: BTreeMap<_, _> = with_w
        .packages_dev
        .iter()
        .chain(with_w.packages.iter())
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    assert_eq!(w.get("acme/phpunit"), Some(&"1.0.0"));
    assert_eq!(w.get("acme/phpunit-util"), Some(&"1.0.0"));

    // -W: frees root require-dev util via require-dev edge → both update
    let with_all = resolve(
        &manifest,
        &opts(&["acme/phpunit"], UpdateDeps::WithAllDependencies),
        tmp.path(),
        Some(&existing),
    )
    .await
    .expect("-W dev-tree resolve");
    let wall: BTreeMap<_, _> = with_all
        .packages_dev
        .iter()
        .chain(with_all.packages.iter())
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    assert_eq!(wall.get("acme/phpunit"), Some(&"2.0.0"));
    assert_eq!(wall.get("acme/phpunit-util"), Some(&"2.0.0"));
}
