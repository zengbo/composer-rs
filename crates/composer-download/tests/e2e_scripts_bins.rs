//! E2E: path install → vendor/bin link + post-autoload-dump script.

use composer_autoload::{AutoloadOptions, generate};
use composer_cache::CasCache;
use composer_download::{PackageInstaller, install_bins};
use composer_manifest::ComposerJson;
use composer_resolver::{ResolveOptions, resolve};
use composer_scripts::{ScriptEvent, run_event};
use std::fs;

fn write_file(path: &std::path::Path, body: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, body).unwrap();
}

#[tokio::test]
async fn e2e_path_install_bins_and_post_autoload_script() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let tool = root.join("packages/acme-tool");
    write_file(
        &tool.join("composer.json"),
        r#"{
            "name": "acme/tool",
            "version": "1.0.0",
            "type": "library",
            "bin": ["bin/hello"],
            "autoload": { "psr-4": { "Acme\\Tool\\": "src/" } }
        }"#,
    );
    write_file(&tool.join("bin/hello"), "#!/bin/sh\necho hello-from-bin\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(tool.join("bin/hello")).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(tool.join("bin/hello"), perms).unwrap();
    }
    write_file(
        &tool.join("src/Hi.php"),
        "<?php\nnamespace Acme\\Tool;\nclass Hi {}\n",
    );

    let app = root.join("app");
    write_file(
        &app.join("composer.json"),
        r#"{
            "name": "acme/app",
            "require": { "php": ">=8.0", "acme/tool": "*" },
            "repositories": [
                { "type": "path", "url": "../packages/acme-tool" }
            ],
            "config": { "platform": { "php": "8.2.0" } },
            "scripts": {
                "post-autoload-dump": "echo dumped > script-ran.txt"
            }
        }"#,
    );

    let composer_json_path = app.join("composer.json");
    let composer_json_bytes = fs::read(&composer_json_path).unwrap();
    let manifest = ComposerJson::from_str(std::str::from_utf8(&composer_json_bytes).unwrap()).unwrap();
    let options = ResolveOptions {
        with_dev: false,
        prefer_stable: true,
        prefer_lowest: false,
        minimum_stability: "stable".into(),
        concurrency: 4,
        ignore_platform_reqs: false,
        ignore_platform_req: Vec::new(),
        packages_to_update: Vec::new(),
        update_deps: Default::default(),
    };
    let resolution = resolve(&manifest, &options, &app, None)
        .await
        .expect("resolve");
    let lock = resolution.to_lock(&manifest, &composer_json_bytes);
    let vendor = app.join("vendor");
    fs::create_dir_all(&vendor).unwrap();

    let installer = PackageInstaller::new(4, false)
        .unwrap()
        .with_project_root(&app)
        .with_cache(CasCache::with_root(root.join("cache")));
    let packages = composer_resolver::locked_list(&lock, false);
    let refs: Vec<_> = packages.iter().collect();
    installer
        .install_all(&refs, &vendor)
        .await
        .expect("install");

    let bin_dir = app.join(manifest.bin_dir());
    let bins = install_bins(&refs, &vendor, &app, &bin_dir, &manifest.installer_paths()).unwrap();
    assert_eq!(bins.linked, 1);
    assert!(bin_dir.join("hello").exists() || bin_dir.join("hello").is_symlink());

    generate(
        &app,
        &vendor,
        &manifest,
        Some(&lock),
        &AutoloadOptions {
            optimize: false,
            classmap_authoritative: false,
            with_dev: false,
        },
    )
    .unwrap();

    run_event(&manifest, ScriptEvent::PostAutoloadDump, &app, true).expect("post-autoload-dump");
    let ran = fs::read_to_string(app.join("script-ran.txt")).expect("script output");
    assert!(ran.contains("dumped"));
    assert!(vendor.join("autoload.php").is_file());
}
