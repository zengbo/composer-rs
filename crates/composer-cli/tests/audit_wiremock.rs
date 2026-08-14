//! Offline audit: mock Packagist security-advisories API.

use composer_core::ComposerVersion;
use composer_lock::{ComposerLock, DistInfo, LockedPackage};
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn locked(name: &str, ver: &str) -> LockedPackage {
    LockedPackage {
        name: name.into(),
        version: ver.into(),
        source: None,
        dist: Some(DistInfo {
            dist_type: "zip".into(),
            url: format!("https://example.com/{name}.zip"),
            reference: None,
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
        unknown: BTreeMap::new(),
    }
}

#[test]
fn advisory_matcher_hits_locked_vulnerable_version() {
    let v = ComposerVersion::parse("1.2.3").unwrap();
    assert!(composer_cli::commands::audit::version_in_affected(
        &v, "^1.0"
    ));
    assert!(!composer_cli::commands::audit::version_in_affected(
        &v, "^2.0"
    ));
}

#[tokio::test]
async fn audit_command_fails_when_advisory_matches_lock() {
    let _g = ENV_LOCK.lock().unwrap();
    let server = MockServer::start().await;

    let body = r#"{
        "advisories": {
            "acme/vuln": [
                {
                    "advisoryId": "PKSA-123",
                    "title": "RCE",
                    "severity": "high",
                    "cve": ["CVE-2024-0001"],
                    "affectedVersions": "^1.0",
                    "link": "https://example.com/cve"
                }
            ]
        }
    }"#;

    Mock::given(method("GET"))
        .and(path("/api/security-advisories/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("composer.json"),
        r#"{"name":"acme/app","require":{"acme/vuln":"^1.0"}}"#,
    )
    .unwrap();
    let lock = ComposerLock {
        packages: vec![locked("acme/vuln", "1.2.3")],
        ..Default::default()
    };
    lock.save(&tmp.path().join("composer.lock")).unwrap();

    let prev = std::env::var_os("COMPOSER_RS_AUDIT_URL");
    unsafe {
        std::env::set_var(
            "COMPOSER_RS_AUDIT_URL",
            format!("{}/api/security-advisories/", server.uri()),
        );
    }

    let bin = env!("CARGO_BIN_EXE_composer-rs");
    let status = Command::new(bin)
        .args(["audit", "--format=json"])
        .current_dir(tmp.path())
        .status()
        .expect("spawn audit");

    unsafe {
        match prev {
            Some(v) => std::env::set_var("COMPOSER_RS_AUDIT_URL", v),
            None => std::env::remove_var("COMPOSER_RS_AUDIT_URL"),
        }
    }

    assert!(!status.success(), "audit should fail when CVE matches lock");
}

#[tokio::test]
async fn audit_command_fails_when_advisory_service_is_unavailable() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("composer.json"),
        r#"{"name":"acme/app","require":{"acme/lib":"^1.0"}}"#,
    )
    .unwrap();
    let lock = ComposerLock {
        packages: vec![locked("acme/lib", "1.0.0")],
        ..Default::default()
    };
    lock.save(&tmp.path().join("composer.lock")).unwrap();

    let prev = std::env::var_os("COMPOSER_RS_AUDIT_URL");
    unsafe {
        std::env::set_var("COMPOSER_RS_AUDIT_URL", "http://127.0.0.1:9/");
    }

    let bin = env!("CARGO_BIN_EXE_composer-rs");
    let status = Command::new(bin)
        .arg("audit")
        .current_dir(tmp.path())
        .status()
        .expect("spawn audit");

    unsafe {
        match prev {
            Some(v) => std::env::set_var("COMPOSER_RS_AUDIT_URL", v),
            None => std::env::remove_var("COMPOSER_RS_AUDIT_URL"),
        }
    }

    assert!(
        !status.success(),
        "audit must fail closed when the advisory service is unreachable"
    );
}
