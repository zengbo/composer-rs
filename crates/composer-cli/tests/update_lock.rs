//! Integration: `update --lock` refreshes content-hash without changing packages.

use composer_lock::ComposerLock;
use composer_manifest::content_hash;
use std::process::Command;

#[test]
fn update_lock_rewrites_content_hash_only() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let manifest_json = r#"{
        "name": "acme/app",
        "require": { "php": ">=8.0" },
        "config": { "platform": { "php": "8.2.0" } }
    }"#;
    std::fs::write(root.join("composer.json"), manifest_json).unwrap();

    let mut lock = ComposerLock::default();
    lock.content_hash = "deadbeefdeadbeefdeadbeefdeadbeef".into();
    lock.packages = vec![];
    lock.save(&root.join("composer.lock")).unwrap();

    let bin = env!("CARGO_BIN_EXE_composer-rs");
    let status = Command::new(bin)
        .args(["update", "--lock"])
        .current_dir(root)
        .status()
        .expect("spawn composer-rs");
    assert!(status.success(), "update --lock should succeed");

    let updated = ComposerLock::load(&root.join("composer.lock")).unwrap();
    let expected = content_hash(manifest_json.as_bytes()).unwrap();
    assert_eq!(updated.content_hash, expected);
    assert!(updated.packages.is_empty());
}
