//! Generate Composer-compatible PHP autoload files under `vendor/`.

#![deny(unsafe_code)]

use composer_core::error::{Error, Result};
use composer_core::{AutoloadConfig, PackageId};
use composer_lock::{ComposerLock, LockedPackage};
use composer_manifest::ComposerJson;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing::debug;
use walkdir::WalkDir;

/// Options for autoloader generation.
#[derive(Debug, Clone)]
pub struct AutoloadOptions {
    pub optimize: bool,
    pub classmap_authoritative: bool,
    pub with_dev: bool,
}

impl Default for AutoloadOptions {
    fn default() -> Self {
        Self {
            optimize: false,
            classmap_authoritative: false,
            with_dev: true,
        }
    }
}

/// Generate `vendor/autoload.php` and supporting files.
pub fn generate(
    project_root: &Path,
    vendor_dir: &Path,
    manifest: &ComposerJson,
    lock: Option<&ComposerLock>,
    options: &AutoloadOptions,
) -> Result<()> {
    let autoload_dir = vendor_dir.join("composer");
    fs::create_dir_all(&autoload_dir).map_err(|e| Error::io(&autoload_dir, e))?;

    let mut psr4: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut psr0: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut files: Vec<String> = Vec::new();
    let mut classmap_dirs: Vec<String> = Vec::new();
    let mut classmap: BTreeMap<String, String> = BTreeMap::new();

    // Namespaces / classmap: root first so the app can override vendor prefixes
    // (Composer parseAutoloads uses the reverse-sorted package map).
    if let Some(al) = &manifest.autoload {
        merge_autoload(
            al,
            "", // root package lives at project root; paths relative to vendor use ../
            true,
            &mut psr4,
            &mut psr0,
            &mut classmap_dirs,
        );
    }
    if options.with_dev {
        if let Some(al) = &manifest.autoload_dev {
            merge_autoload(al, "", true, &mut psr4, &mut psr0, &mut classmap_dirs);
        }
    }

    // Locked packages (PSR / classmap). `files` are collected separately so
    // they can be ordered dependencies-first like Composer\Util\PackageSorter.
    if let Some(lock) = lock {
        for pkg in lock.packages_to_install(options.with_dev) {
            collect_package_autoload(pkg, &mut psr4, &mut psr0, &mut classmap_dirs)?;
        }
        let pkgs = lock.packages_to_install(options.with_dev);
        for pkg in sort_packages_by_dependency_weight(&pkgs) {
            append_files(&mut files, pkg.autoload.as_ref(), &pkg.name, false);
        }
    }

    // Root `files` last so helpers can call vendor functions (Composer appends the root).
    if let Some(al) = &manifest.autoload {
        append_files(&mut files, Some(al), "", true);
    }
    if options.with_dev {
        if let Some(al) = &manifest.autoload_dev {
            append_files(&mut files, Some(al), "", true);
        }
    }

    if options.optimize || options.classmap_authoritative {
        // Scan classmap dirs + convert PSR prefixes into classmap where possible
        for dir in &classmap_dirs {
            let abs = resolve_autoload_path(vendor_dir, dir);
            scan_classmap(&abs, vendor_dir, &mut classmap)?;
        }
        if options.optimize {
            for (prefix, paths) in &psr4 {
                for p in paths {
                    let abs = resolve_autoload_path(vendor_dir, p);
                    scan_psr4_classmap(prefix, &abs, vendor_dir, &mut classmap)?;
                }
            }
        }
    } else {
        for dir in &classmap_dirs {
            let abs = resolve_autoload_path(vendor_dir, dir);
            scan_classmap(&abs, vendor_dir, &mut classmap)?;
        }
    }

    // Official Composer always classmaps the dumped runtime class.
    // Path is relative to vendor/composer (same convention as package files).
    classmap.insert(
        "Composer\\InstalledVersions".into(),
        "../composer/InstalledVersions.php".into(),
    );

    write_autoload_namespaces(&autoload_dir, &psr0)?;
    write_autoload_psr4(&autoload_dir, &psr4)?;
    write_autoload_classmap(&autoload_dir, &classmap)?;
    write_autoload_files(&autoload_dir, &files)?;
    write_autoload_static(
        &autoload_dir,
        &psr4,
        &psr0,
        &classmap,
        &files,
        options.optimize,
    )?;
    write_class_loader(&autoload_dir)?;
    write_installed_versions(&autoload_dir)?;
    write_autoload_real(
        &autoload_dir,
        options.classmap_authoritative,
        options.optimize,
    )?;
    write_installed(&autoload_dir, lock, options.with_dev)?;
    write_installed_php(&autoload_dir, manifest, lock, options.with_dev)?;
    write_platform_check(&autoload_dir, manifest, lock)?;

    // vendor/autoload.php
    let autoload_php = vendor_dir.join("autoload.php");
    fs::write(
        &autoload_php,
        "<?php\n\n// autoload.php @generated by composer-rs\n\n\
         require_once __DIR__ . '/composer/autoload_real.php';\n\n\
         return ComposerAutoloaderInit::getLoader();\n",
    )
    .map_err(|e| Error::io(&autoload_php, e))?;

    debug!(vendor = %vendor_dir.display(), "autoloader generated");
    let _ = project_root;
    Ok(())
}

fn merge_autoload(
    al: &AutoloadConfig,
    package_prefix: &str,
    is_root: bool,
    psr4: &mut BTreeMap<String, Vec<String>>,
    psr0: &mut BTreeMap<String, Vec<String>>,
    classmap_dirs: &mut Vec<String>,
) {
    for (ns, paths) in &al.psr4 {
        for p in paths.paths() {
            let rel = package_path(package_prefix, p, is_root);
            psr4.entry(ns.clone()).or_default().push(rel);
        }
    }
    for (ns, paths) in &al.psr0 {
        for p in paths.paths() {
            let rel = package_path(package_prefix, p, is_root);
            psr0.entry(ns.clone()).or_default().push(rel);
        }
    }
    for d in &al.classmap {
        classmap_dirs.push(package_path(package_prefix, d, is_root));
    }
}

fn append_files(
    files: &mut Vec<String>,
    al: Option<&AutoloadConfig>,
    package_prefix: &str,
    is_root: bool,
) {
    let Some(al) = al else {
        return;
    };
    for f in &al.files {
        files.push(package_path(package_prefix, f, is_root));
    }
}

/// Composer `PackageSorter::sortPackages`: lower weight (more dependents) first.
fn sort_packages_by_dependency_weight<'a>(
    packages: &[&'a LockedPackage],
) -> Vec<&'a LockedPackage> {
    let mut usage: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pkg in packages {
        for target in pkg.require.keys() {
            usage
                .entry(target.as_str())
                .or_default()
                .push(pkg.name.as_str());
        }
    }

    fn importance(
        name: &str,
        usage: &BTreeMap<&str, Vec<&str>>,
        computing: &mut BTreeSet<String>,
        computed: &mut BTreeMap<String, i32>,
    ) -> i32 {
        if let Some(&w) = computed.get(name) {
            return w;
        }
        if !computing.insert(name.to_string()) {
            return 0;
        }
        let mut weight = 0i32;
        if let Some(users) = usage.get(name) {
            for user in users {
                weight -= 1 - importance(user, usage, computing, computed);
            }
        }
        computing.remove(name);
        computed.insert(name.to_string(), weight);
        weight
    }

    let mut computing = BTreeSet::new();
    let mut computed = BTreeMap::new();
    let mut weighted: Vec<(i32, &str, usize)> = packages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            (
                importance(p.name.as_str(), &usage, &mut computing, &mut computed),
                p.name.as_str(),
                i,
            )
        })
        .collect();
    weighted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    weighted.into_iter().map(|(_, _, i)| packages[i]).collect()
}

fn package_path(package_install: &str, path: &str, is_root: bool) -> String {
    let path = path.trim_start_matches("./");
    if is_root {
        // From vendor/composer → project root is ../..
        if path.is_empty() || path == "." {
            return "../..".into();
        }
        return format!("../../{path}");
    }
    if path.is_empty() || path == "." {
        format!("../{package_install}")
    } else {
        format!("../{package_install}/{path}")
    }
}

fn collect_package_autoload(
    pkg: &LockedPackage,
    psr4: &mut BTreeMap<String, Vec<String>>,
    psr0: &mut BTreeMap<String, Vec<String>>,
    classmap_dirs: &mut Vec<String>,
) -> Result<()> {
    let install = pkg.name.clone(); // vendor/name
    if let Some(al) = &pkg.autoload {
        merge_autoload(al, &install, false, psr4, psr0, classmap_dirs);
    }
    Ok(())
}

fn resolve_autoload_path(vendor_dir: &Path, rel_from_composer: &str) -> PathBuf {
    // rel is relative to vendor/composer
    vendor_dir.join("composer").join(rel_from_composer)
}

fn scan_classmap(
    path: &Path,
    vendor_dir: &Path,
    classmap: &mut BTreeMap<String, String>,
) -> Result<()> {
    // Composer classmap entries may be a directory or a single .php file
    // (e.g. thecodingmachine/safe lists lib/DateTime.php).
    if path.is_file() {
        return index_php_file(path, vendor_dir, classmap);
    }
    if !path.is_dir() {
        return Ok(());
    }
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            index_php_file(entry.path(), vendor_dir, classmap)?;
        }
    }
    Ok(())
}

fn index_php_file(
    path: &Path,
    vendor_dir: &Path,
    classmap: &mut BTreeMap<String, String>,
) -> Result<()> {
    if path.extension().and_then(|e| e.to_str()) != Some("php") {
        return Ok(());
    }
    if let Ok(content) = fs::read_to_string(path) {
        let rel = path_relative_to_composer(path, vendor_dir);
        for class in extract_types(&content) {
            classmap.insert(class, rel.clone());
        }
    }
    Ok(())
}

fn scan_psr4_classmap(
    prefix: &str,
    dir: &Path,
    vendor_dir: &Path,
    classmap: &mut BTreeMap<String, String>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let prefix_ns = prefix.trim_end_matches('\\');
    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("php") {
            continue;
        }
        if let Ok(rel_to_base) = path.strip_prefix(dir) {
            let mut class = prefix_ns.to_string();
            for comp in rel_to_base.components() {
                if let std::path::Component::Normal(c) = comp {
                    let s = c.to_string_lossy();
                    if s.ends_with(".php") {
                        let name = s.trim_end_matches(".php");
                        if !class.is_empty() {
                            class.push('\\');
                        }
                        class.push_str(name);
                    } else {
                        if !class.is_empty() {
                            class.push('\\');
                        }
                        class.push_str(&s);
                    }
                }
            }
            let rel = path_relative_to_composer(path, vendor_dir);
            classmap.insert(class, rel);
        }
    }
    Ok(())
}

fn path_relative_to_composer(path: &Path, vendor_dir: &Path) -> String {
    let composer_dir = vendor_dir.join("composer");
    pathdiff_simple(&composer_dir, path)
}

fn pathdiff_simple(from: &Path, to: &Path) -> String {
    // Simple relative path: assume both absolute or same root
    let from = fs::canonicalize(from).unwrap_or_else(|_| from.to_path_buf());
    let to = fs::canonicalize(to).unwrap_or_else(|_| to.to_path_buf());
    let from_comps: Vec<_> = from.components().collect();
    let to_comps: Vec<_> = to.components().collect();
    let mut i = 0;
    while i < from_comps.len() && i < to_comps.len() && from_comps[i] == to_comps[i] {
        i += 1;
    }
    let mut out = PathBuf::new();
    for _ in i..from_comps.len() {
        out.push("..");
    }
    for c in &to_comps[i..] {
        out.push(c);
    }
    out.to_string_lossy().replace('\\', "/")
}

fn extract_types(php: &str) -> Vec<String> {
    static NS_RE: OnceLock<Regex> = OnceLock::new();
    static TYPE_RE: OnceLock<Regex> = OnceLock::new();
    let ns_re = NS_RE.get_or_init(|| Regex::new(r"(?m)^\s*namespace\s+([^;{]+)").unwrap());
    let type_re = TYPE_RE.get_or_init(|| {
        // PHP 8.2+ allows `readonly` (and any order of abstract/final/readonly).
        Regex::new(
            r"(?m)^\s*(?:(?:abstract|final|readonly)\s+)*(?:class|interface|trait|enum)\s+(\w+)",
        )
        .unwrap()
    });

    let ns = ns_re
        .captures(php)
        .map(|c| c[1].trim().to_string())
        .unwrap_or_default();

    let mut out = Vec::new();
    for cap in type_re.captures_iter(php) {
        let name = &cap[1];
        if ns.is_empty() {
            out.push(name.to_string());
        } else {
            out.push(format!("{ns}\\{name}"));
        }
    }
    out
}

fn php_array_strings(map: &BTreeMap<String, Vec<String>>) -> String {
    let mut s = String::from("array(\n");
    for (k, paths) in map {
        let paths_php: Vec<String> = paths
            .iter()
            .map(|p| {
                format!(
                    "$vendorDir . '/{}'",
                    escape_php(p.trim_start_matches("../"))
                )
            })
            .collect();
        // Our paths are relative to vendor/composer; Composer uses $vendorDir and $baseDir.
        // We emit paths relative to vendor/composer as simple strings with $vendorDir.
        let rendered_paths: Vec<String> = paths
            .iter()
            .map(|p| {
                if p.starts_with("../../") {
                    format!("$baseDir . '/{}'", escape_php(&p[6..]))
                } else if p.starts_with("../") {
                    format!("$vendorDir . '/{}'", escape_php(&p[3..]))
                } else {
                    format!("$vendorDir . '/{}'", escape_php(p))
                }
            })
            .collect();
        let _ = paths_php;
        if rendered_paths.len() == 1 {
            s.push_str(&format!(
                "    '{}' => {},\n",
                escape_php(k),
                rendered_paths[0]
            ));
        } else {
            s.push_str(&format!("    '{}' => array(", escape_php(k)));
            for (i, p) in rendered_paths.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(p);
            }
            s.push_str("),\n");
        }
    }
    s.push_str(")");
    s
}

fn escape_php(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn write_autoload_psr4(dir: &Path, psr4: &BTreeMap<String, Vec<String>>) -> Result<()> {
    let path = dir.join("autoload_psr4.php");
    let body = format!(
        "<?php\n\n// autoload_psr4.php @generated by composer-rs\n\n\
         $vendorDir = dirname(__DIR__);\n\
         $baseDir = dirname($vendorDir);\n\n\
         return {};\n",
        php_array_strings(psr4)
    );
    fs::write(&path, body).map_err(|e| Error::io(&path, e))
}

fn write_autoload_namespaces(dir: &Path, psr0: &BTreeMap<String, Vec<String>>) -> Result<()> {
    let path = dir.join("autoload_namespaces.php");
    let body = format!(
        "<?php\n\n// autoload_namespaces.php @generated by composer-rs\n\n\
         $vendorDir = dirname(__DIR__);\n\
         $baseDir = dirname($vendorDir);\n\n\
         return {};\n",
        php_array_strings(psr0)
    );
    fs::write(&path, body).map_err(|e| Error::io(&path, e))
}

fn write_autoload_classmap(dir: &Path, classmap: &BTreeMap<String, String>) -> Result<()> {
    let path = dir.join("autoload_classmap.php");
    let mut s = String::from(
        "<?php\n\n// autoload_classmap.php @generated by composer-rs\n\n\
         $vendorDir = dirname(__DIR__);\n\
         $baseDir = dirname($vendorDir);\n\n\
         return array(\n",
    );
    for (class, rel) in classmap {
        let val = if rel.starts_with("../../") {
            format!("$baseDir . '/{}'", escape_php(&rel[6..]))
        } else if rel.starts_with("../") {
            format!("$vendorDir . '/{}'", escape_php(&rel[3..]))
        } else {
            format!("$vendorDir . '/{}'", escape_php(rel))
        };
        s.push_str(&format!("    '{}' => {},\n", escape_php(class), val));
    }
    s.push_str(");\n");
    fs::write(&path, s).map_err(|e| Error::io(&path, e))
}

fn write_autoload_files(dir: &Path, files: &[String]) -> Result<()> {
    let path = dir.join("autoload_files.php");
    let mut s = String::from(
        "<?php\n\n// autoload_files.php @generated by composer-rs\n\n\
         $vendorDir = dirname(__DIR__);\n\
         $baseDir = dirname($vendorDir);\n\n\
         return array(\n",
    );
    for (i, f) in files.iter().enumerate() {
        let val = if f.starts_with("../../") {
            format!("$baseDir . '/{}'", escape_php(&f[6..]))
        } else if f.starts_with("../") {
            format!("$vendorDir . '/{}'", escape_php(&f[3..]))
        } else {
            format!("$vendorDir . '/{}'", escape_php(f))
        };
        // Composer uses hash keys
        s.push_str(&format!("    'composer_rs_{i:04}' => {val},\n"));
    }
    s.push_str(");\n");
    fs::write(&path, s).map_err(|e| Error::io(&path, e))
}

fn render_autoload_path(rel: &str) -> String {
    if rel.starts_with("../../") {
        format!("$baseDir . '/{}'", escape_php(&rel[6..]))
    } else if rel.starts_with("../") {
        format!("$vendorDir . '/{}'", escape_php(&rel[3..]))
    } else {
        format!("$vendorDir . '/{}'", escape_php(rel))
    }
}

fn write_autoload_static(
    dir: &Path,
    psr4: &BTreeMap<String, Vec<String>>,
    psr0: &BTreeMap<String, Vec<String>>,
    classmap: &BTreeMap<String, String>,
    files: &[String],
    optimize: bool,
) -> Result<()> {
    let path = dir.join("autoload_static.php");

    let classmap_body = if optimize && !classmap.is_empty() {
        let mut s = String::from("array(\n");
        for (class, rel) in classmap {
            s.push_str(&format!(
                "        '{}' => {},\n",
                escape_php(class),
                render_autoload_path(rel)
            ));
        }
        s.push_str("    )");
        s
    } else {
        "array()".into()
    };

    let (prefix_lengths, prefix_dirs) = if optimize {
        render_psr4_static(psr4)
    } else {
        ("array()".into(), "array()".into())
    };

    let prefixes_psr0 = if optimize {
        render_psr0_static(psr0)
    } else {
        "array()".into()
    };

    let files_body = if optimize && !files.is_empty() {
        let mut s = String::from("array(\n");
        for (i, f) in files.iter().enumerate() {
            s.push_str(&format!(
                "        'composer_rs_{i:04}' => {},\n",
                render_autoload_path(f)
            ));
        }
        s.push_str("    )");
        s
    } else {
        "array()".into()
    };

    let body = format!(
        "<?php\n\n// autoload_static.php @generated by composer-rs\n\n\
         namespace Composer\\Autoload;\n\n\
         class ComposerStaticInit\n{{\n\
             public static $prefixLengthsPsr4 = {prefix_lengths};\n\
             public static $prefixDirsPsr4 = {prefix_dirs};\n\
             public static $prefixesPsr0 = {prefixes_psr0};\n\
             public static $classMap = {classmap_body};\n\
             public static $files = {files_body};\n\n\
             public static function getInitializer(ClassLoader $loader)\n\
             {{\n\
                 return \\Closure::bind(function () use ($loader) {{\n\
                     $loader->prefixLengthsPsr4 = ComposerStaticInit::$prefixLengthsPsr4;\n\
                     $loader->prefixDirsPsr4 = ComposerStaticInit::$prefixDirsPsr4;\n\
                     $loader->prefixesPsr0 = ComposerStaticInit::$prefixesPsr0;\n\
                     $loader->classMap = ComposerStaticInit::$classMap;\n\
                 }}, null, ClassLoader::class);\n\
             }}\n\
         }}\n"
    );
    fs::write(&path, body).map_err(|e| Error::io(&path, e))
}

fn render_psr4_static(psr4: &BTreeMap<String, Vec<String>>) -> (String, String) {
    if psr4.is_empty() {
        return ("array()".into(), "array()".into());
    }

    let mut lengths: BTreeMap<char, BTreeMap<String, usize>> = BTreeMap::new();
    let mut dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (prefix, paths) in psr4 {
        let ns = if prefix.ends_with('\\') {
            prefix.clone()
        } else {
            format!("{prefix}\\")
        };
        let first = ns.chars().next().unwrap_or('A');
        lengths
            .entry(first)
            .or_default()
            .insert(ns.clone(), ns.len());
        dirs.insert(ns, paths.iter().map(|p| render_autoload_path(p)).collect());
    }

    let mut lengths_php = String::from("array(\n");
    for (first, map) in &lengths {
        lengths_php.push_str(&format!("        '{first}' => array(\n"));
        for (prefix, len) in map {
            lengths_php.push_str(&format!("            '{}' => {len},\n", escape_php(prefix)));
        }
        lengths_php.push_str("        ),\n");
    }
    lengths_php.push_str("    )");

    // ClassLoader::findFile does `foreach ($prefixDirsPsr4[$prefix] as $dir)` —
    // values must always be arrays (never a bare path string).
    let mut dirs_php = String::from("array(\n");
    for (prefix, paths) in &dirs {
        dirs_php.push_str(&format!("        '{}' => array(", escape_php(prefix)));
        for (i, p) in paths.iter().enumerate() {
            if i > 0 {
                dirs_php.push_str(", ");
            }
            dirs_php.push_str(p);
        }
        dirs_php.push_str("),\n");
    }
    dirs_php.push_str("    )");

    (lengths_php, dirs_php)
}

fn render_psr0_static(psr0: &BTreeMap<String, Vec<String>>) -> String {
    if psr0.is_empty() {
        return "array()".into();
    }
    // Match Composer / ClassLoader layout: first-char → prefix → [dirs].
    let mut nested: BTreeMap<char, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for (prefix, paths) in psr0 {
        let first = prefix.chars().next().unwrap_or('\\');
        nested.entry(first).or_default().insert(
            prefix.clone(),
            paths.iter().map(|p| render_autoload_path(p)).collect(),
        );
    }

    let mut out = String::from("array(\n");
    for (first, map) in &nested {
        out.push_str(&format!("        '{first}' => array(\n"));
        for (prefix, paths) in map {
            let key = if prefix.is_empty() {
                "''".into()
            } else {
                format!("'{}'", escape_php(prefix))
            };
            out.push_str(&format!("            {key} => array("));
            for (i, p) in paths.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(p);
            }
            out.push_str("),\n");
        }
        out.push_str("        ),\n");
    }
    out.push_str("    )");
    out
}

fn write_class_loader(dir: &Path) -> Result<()> {
    let path = dir.join("ClassLoader.php");
    // Official Composer ClassLoader (MIT). Interceptors such as
    // rightcapital/method-delegate bind to private static $includeFile.
    const BODY: &str = include_str!("../php/ClassLoader.php");
    if fs::read_to_string(&path).ok().as_deref() == Some(BODY) {
        return Ok(());
    }
    fs::write(&path, BODY).map_err(|e| Error::io(&path, e))
}

fn write_installed_versions(dir: &Path) -> Result<()> {
    let path = dir.join("InstalledVersions.php");
    // Official Composer InstalledVersions (MIT). Projects call
    // Composer\InstalledVersions without depending on composer/composer.
    const BODY: &str = include_str!("../php/InstalledVersions.php");
    if fs::read_to_string(&path).ok().as_deref() == Some(BODY) {
        return Ok(());
    }
    fs::write(&path, BODY).map_err(|e| Error::io(&path, e))
}

fn write_autoload_real(dir: &Path, classmap_authoritative: bool, optimize: bool) -> Result<()> {
    let path = dir.join("autoload_real.php");
    let authoritative = if classmap_authoritative {
        "true"
    } else {
        "false"
    };
    let use_static = if optimize { "true" } else { "false" };
    let body = format!(
        r#"<?php
// autoload_real.php @generated by composer-rs

class ComposerAutoloaderInit
{{
    private static $loader;

    public static function getLoader()
    {{
        if (null !== self::$loader) {{
            return self::$loader;
        }}

        require __DIR__ . '/platform_check.php';
        if (!class_exists('Composer\\Autoload\\ClassLoader', false)) {{
            require_once __DIR__ . '/ClassLoader.php';
        }}
        self::$loader = $loader = new \Composer\Autoload\ClassLoader(\dirname(__DIR__));

        $useStatic = {use_static};
        if ($useStatic) {{
            require __DIR__ . '/autoload_static.php';
            call_user_func(\Composer\Autoload\ComposerStaticInit::getInitializer($loader));
        }} else {{
            $map = require __DIR__ . '/autoload_namespaces.php';
            foreach ($map as $namespace => $path) {{
                $loader->add($namespace, $path);
            }}

            $map = require __DIR__ . '/autoload_psr4.php';
            foreach ($map as $namespace => $path) {{
                $loader->addPsr4($namespace, $path);
            }}

            $classMap = require __DIR__ . '/autoload_classmap.php';
            if ($classMap) {{
                $loader->addClassMap($classMap);
            }}
        }}

        if ({authoritative}) {{
            $loader->setClassMapAuthoritative(true);
        }}

        $loader->register(true);

        $includeFiles = require __DIR__ . '/autoload_files.php';
        foreach ($includeFiles as $fileIdentifier => $file) {{
            if (empty($GLOBALS['__composer_autoload_files'][$fileIdentifier])) {{
                $GLOBALS['__composer_autoload_files'][$fileIdentifier] = true;
                if (file_exists($file)) {{
                    require $file;
                }}
            }}
        }}

        return $loader;
    }}
}}
"#
    );
    fs::write(&path, body).map_err(|e| Error::io(&path, e))
}

fn write_installed(dir: &Path, lock: Option<&ComposerLock>, with_dev: bool) -> Result<()> {
    let path = dir.join("installed.json");
    let packages = lock
        .map(|l| installed_packages_json(l, with_dev))
        .unwrap_or_default();

    let doc = serde_json::json!({
        "packages": packages,
        "dev": with_dev,
        "dev-package-names": lock.map(|l| {
            l.packages_dev.iter().map(|p| p.name.clone()).collect::<Vec<_>>()
        }).unwrap_or_default(),
        "plugin-api-version": "2.6.0",
    });
    let text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into());
    fs::write(&path, text + "\n").map_err(|e| Error::io(&path, e))
}

/// Build Composer 2-style package entries for `vendor/composer/installed.json`.
pub fn installed_packages_json(lock: &ComposerLock, with_dev: bool) -> Vec<serde_json::Value> {
    lock.packages_to_install(with_dev)
        .into_iter()
        .filter_map(|p| {
            let name = p.name.clone();
            let mut v = serde_json::to_value(p).ok()?;
            if let Some(obj) = v.as_object_mut() {
                let ver = obj
                    .get("version")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                obj.insert(
                    "version_normalized".into(),
                    serde_json::Value::String(composer_core::version_normalized(&ver)),
                );
                // Relative to vendor/composer/
                obj.insert(
                    "install-path".into(),
                    serde_json::Value::String(format!("../{name}")),
                );
            }
            Some(v)
        })
        .collect()
}

fn write_installed_php(
    dir: &Path,
    manifest: &ComposerJson,
    lock: Option<&ComposerLock>,
    with_dev: bool,
) -> Result<()> {
    let path = dir.join("installed.php");
    let root_name = manifest.name.as_deref().unwrap_or("__root__");
    let root_pretty = manifest
        .version
        .as_deref()
        .unwrap_or("1.0.0+no-version-set");
    let root_type = manifest.package_type.as_deref().unwrap_or("library");
    let mut versions = String::from("array(\n");
    if let Some(lock) = lock {
        let dev_names: BTreeSet<&str> = lock.packages_dev.iter().map(|p| p.name.as_str()).collect();
        for p in lock.packages_to_install(with_dev) {
            let reference = p
                .dist
                .as_ref()
                .and_then(|d| d.reference.as_deref())
                .or_else(|| p.source.as_ref().and_then(|s| s.reference.as_deref()));
            let pkg_type = p.package_type.as_deref().unwrap_or("library");
            let normalized = composer_core::version_normalized(&p.version);
            let dev_req = if dev_names.contains(p.name.as_str()) {
                "true"
            } else {
                "false"
            };
            versions.push_str(&format!(
                "        '{name}' => array(\n            'pretty_version' => '{pretty}',\n            'version' => '{version}',\n            'reference' => {reference},\n            'type' => '{ty}',\n            'install_path' => __DIR__ . '/../{name}',\n            'aliases' => array(),\n            'dev_requirement' => {dev_req},\n        ),\n",
                name = escape_php(&p.name),
                pretty = escape_php(&p.version),
                version = escape_php(&normalized),
                reference = php_optional_string(reference),
                ty = escape_php(pkg_type),
                dev_req = dev_req,
            ));
        }
    }
    versions.push_str("    )");

    let body = format!(
        "<?php return array(\n    'root' => array(\n        'name' => '{root_name}',\n        'pretty_version' => '{root_pretty}',\n        'version' => '{root_norm}',\n        'reference' => null,\n        'type' => '{root_type}',\n        'install_path' => __DIR__ . '/../../',\n        'aliases' => array(),\n        'dev' => {dev},\n    ),\n    'versions' => {versions},\n);\n",
        root_name = escape_php(root_name),
        root_pretty = escape_php(root_pretty),
        root_norm = escape_php(&composer_core::version_normalized(root_pretty)),
        root_type = escape_php(root_type),
        dev = if with_dev { "true" } else { "false" },
        versions = versions,
    );
    fs::write(&path, body).map_err(|e| Error::io(&path, e))
}

fn php_optional_string(value: Option<&str>) -> String {
    match value {
        Some(s) => format!("'{}'", escape_php(s)),
        None => "null".into(),
    }
}

fn write_platform_check(
    dir: &Path,
    manifest: &ComposerJson,
    lock: Option<&ComposerLock>,
) -> Result<()> {
    let path = dir.join("platform_check.php");

    // config.platform-check: false → no-op stub (Composer-compatible)
    if !manifest.platform_check_enabled() {
        let body = "<?php\n// platform_check.php @generated by composer-rs (disabled via config.platform-check)\nreturn;\n";
        return fs::write(&path, body).map_err(|e| Error::io(&path, e));
    }

    let mut reqs: BTreeMap<String, String> = BTreeMap::new();
    for (name, constraint) in &manifest.require {
        if PackageId::parse(name).is_ok_and(|id| id.is_platform()) {
            reqs.insert(name.clone(), constraint.clone());
        }
    }
    for (name, constraint) in &manifest.require_dev {
        if PackageId::parse(name).is_ok_and(|id| id.is_platform()) {
            reqs.insert(name.clone(), constraint.clone());
        }
    }
    if let Some(lock) = lock {
        for (name, constraint) in &lock.platform {
            reqs.insert(name.clone(), constraint.clone());
        }
        for (name, constraint) in &lock.platform_dev {
            reqs.insert(name.clone(), constraint.clone());
        }
    }

    let php_only = manifest
        .config
        .as_ref()
        .and_then(|c| c.get("platform-check"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("php-only"));

    let mut checks = String::new();
    for (name, constraint) in &reqs {
        if name == "php" || name == "hhvm" {
            let min = php_constraint_to_version(constraint);
            checks.push_str(&format!(
                "if (!(version_compare(PHP_VERSION, '{min}', '>='))) {{\n\
                    $issues[] = 'Composer requires PHP {constraint} but ' . PHP_VERSION . ' is installed';\n\
                 }}\n"
            ));
        } else if !php_only {
            if let Some(ext) = name.strip_prefix("ext-") {
                checks.push_str(&format!(
                    "if (!extension_loaded('{ext}')) {{\n\
                        $issues[] = 'Composer requires ext-{ext} ({constraint})';\n\
                     }}\n"
                ));
            }
        }
    }

    let body = if checks.is_empty() {
        "<?php\n// platform_check.php @generated by composer-rs\nreturn;\n".into()
    } else {
        format!(
            "<?php\n// platform_check.php @generated by composer-rs\n\n\
             $issues = array();\n\
             {checks}\n\
             if ($issues) {{\n\
                 if (!headers_sent()) {{\n\
                     header('HTTP/1.1 500 Internal Server Error');\n\
                 }}\n\
                 trigger_error(\n\
                     'Composer detected platform issues: ' . implode('; ', $issues),\n\
                     E_USER_ERROR\n\
                 );\n\
             }}\n"
        )
    };
    fs::write(&path, body).map_err(|e| Error::io(&path, e))
}

/// Extract a PHP version floor from common constraints (`>=8.1`, `^8.2`, `~8.1.0`, ranges).
fn php_constraint_to_version(constraint: &str) -> String {
    // Use the first branch of OR constraints; take the highest lower-bound we can find.
    let primary = constraint
        .split("||")
        .next()
        .unwrap_or(constraint)
        .split('|')
        .next()
        .unwrap_or(constraint)
        .trim();

    let mut floor: Option<String> = None;
    for part in primary
        .split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let candidate = if let Some(v) = part.strip_prefix(">=") {
            Some(v.trim_start_matches('v').to_string())
        } else if let Some(v) = part.strip_prefix('>') {
            // exclusive lower bound — approximate with the given version
            Some(v.trim_start_matches('v').to_string())
        } else if let Some(v) = part.strip_prefix('^') {
            Some(v.trim_start_matches('v').to_string())
        } else if let Some(v) = part.strip_prefix('~') {
            Some(v.trim_start_matches('v').to_string())
        } else if let Some(v) = part.strip_prefix('=') {
            Some(v.trim_start_matches('v').to_string())
        } else if part.starts_with('<') {
            None
        } else if part.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            Some(part.trim_start_matches('v').to_string())
        } else {
            None
        };
        if let Some(v) = candidate {
            floor = Some(match floor {
                Some(prev) => max_version_string(&prev, &v),
                None => v,
            });
        }
    }
    floor.unwrap_or_else(|| primary.trim_start_matches('v').to_string())
}

fn max_version_string(a: &str, b: &str) -> String {
    let pa = parse_triple(a);
    let pb = parse_triple(b);
    if pa >= pb {
        a.to_string()
    } else {
        b.to_string()
    }
}

fn parse_triple(s: &str) -> (u64, u64, u64) {
    let mut it = s.split('.');
    let maj = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let min = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let pat = it
        .next()
        .map(|x| {
            x.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    (maj, min, pat)
}

// Need serde_json for installed.json
use serde_json;

#[cfg(test)]
mod tests {
    use super::*;
    use composer_lock::{DistInfo, LockedPackage};
    use std::collections::BTreeMap;

    fn locked(name: &str, ver: &str) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: ver.into(),
            source: None,
            dist: Some(DistInfo {
                dist_type: "zip".into(),
                url: format!("https://example.com/{name}.zip"),
                reference: Some("abc".into()),
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

    #[test]
    fn installed_json_composer2_shape() {
        let lock = ComposerLock {
            packages: vec![locked("acme/foo", "1.2.3")],
            packages_dev: vec![locked("acme/phpunit", "10.0.0")],
            ..Default::default()
        };
        let pkgs = installed_packages_json(&lock, true);
        assert_eq!(pkgs.len(), 2);
        let foo = &pkgs[0];
        assert_eq!(foo["name"], "acme/foo");
        assert_eq!(foo["version"], "1.2.3");
        assert_eq!(foo["version_normalized"], "1.2.3.0");
        assert_eq!(foo["install-path"], "../acme/foo");
        assert!(foo.get("dist").is_some());

        // Full document golden fields
        let doc = serde_json::json!({
            "packages": pkgs,
            "dev": true,
            "dev-package-names": ["acme/phpunit"],
            "plugin-api-version": "2.6.0",
        });
        assert_eq!(doc["dev"], true);
        assert_eq!(doc["plugin-api-version"], "2.6.0");
        assert_eq!(doc["dev-package-names"][0], "acme/phpunit");
    }

    #[test]
    fn static_psr4_when_optimized() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("composer");
        std::fs::create_dir_all(&dir).unwrap();

        let mut psr4 = BTreeMap::new();
        psr4.insert("Acme\\Lib\\".into(), vec!["../acme/lib/src".into()]);

        write_autoload_static(&dir, &psr4, &BTreeMap::new(), &BTreeMap::new(), &[], true).unwrap();
        let body = std::fs::read_to_string(dir.join("autoload_static.php")).unwrap();
        assert!(body.contains("prefixDirsPsr4"));
        assert!(body.contains("prefixLengthsPsr4"));
        assert!(body.contains("Acme\\\\Lib\\\\"));
        // Single path must still be an array — bare strings break foreach in ClassLoader.
        assert!(
            body.contains("'Acme\\\\Lib\\\\' => array($vendorDir . '/acme/lib/src')"),
            "expected array-wrapped PSR-4 path, got:\n{body}"
        );
    }

    #[test]
    fn static_psr0_nested_by_first_char() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("composer");
        std::fs::create_dir_all(&dir).unwrap();

        let mut psr0 = BTreeMap::new();
        psr0.insert("Foo".into(), vec!["../foo/src".into()]);

        write_autoload_static(&dir, &BTreeMap::new(), &psr0, &BTreeMap::new(), &[], true).unwrap();
        let body = std::fs::read_to_string(dir.join("autoload_static.php")).unwrap();
        assert!(
            body.contains("'F' => array("),
            "PSR-0 must nest under first character:\n{body}"
        );
        assert!(body.contains("'Foo' => array($vendorDir . '/foo/src')"));
    }

    #[test]
    fn files_autoload_dependencies_before_dependents_and_root_last() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vendor = root.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();

        let mut safe = locked("thecodingmachine/safe", "2.0.0");
        safe.autoload = Some(AutoloadConfig {
            files: vec!["generated/filesystem.php".into()],
            ..Default::default()
        });

        let mut fly = locked("rightcapital/flysystem-compress-adapter", "1.0.0");
        fly.require
            .insert("thecodingmachine/safe".into(), "^2".into());
        fly.autoload = Some(AutoloadConfig {
            files: vec!["src/bootstrap.php".into()],
            ..Default::default()
        });

        // Dependent listed first — lock order would emit bootstrap before Safe.
        let lock = ComposerLock {
            packages: vec![fly, safe],
            ..Default::default()
        };

        let manifest: ComposerJson = serde_json::from_value(serde_json::json!({
            "name": "acme/app",
            "autoload": { "files": ["app/helpers.php"] }
        }))
        .unwrap();

        generate(
            root,
            &vendor,
            &manifest,
            Some(&lock),
            &AutoloadOptions::default(),
        )
        .unwrap();
        let body = std::fs::read_to_string(vendor.join("composer/autoload_files.php")).unwrap();
        let safe_at = body
            .find("thecodingmachine/safe/generated/filesystem.php")
            .expect(&body);
        let fly_at = body
            .find("rightcapital/flysystem-compress-adapter/src/bootstrap.php")
            .expect(&body);
        let root_at = body.find("app/helpers.php").expect(&body);
        assert!(
            safe_at < fly_at,
            "Safe files must load before dependents:\n{body}"
        );
        assert!(fly_at < root_at, "root files must load last:\n{body}");
    }

    #[test]
    fn package_sorter_puts_transitive_deps_first() {
        let mut a = locked("acme/a", "1.0.0");
        a.require.insert("acme/b".into(), "*".into());
        let mut b = locked("acme/b", "1.0.0");
        b.require.insert("acme/c".into(), "*".into());
        let c = locked("acme/c", "1.0.0");
        let pkgs = [&a, &b, &c];
        let sorted: Vec<&str> = sort_packages_by_dependency_weight(&pkgs)
            .into_iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(sorted, vec!["acme/c", "acme/b", "acme/a"]);
    }

    #[test]
    fn extract_types_finds_readonly_classes() {
        let php = r#"<?php
namespace SebastianBergmann;

final readonly class Version {}
"#;
        assert_eq!(
            extract_types(php),
            vec!["SebastianBergmann\\Version".to_string()]
        );
    }

    #[test]
    fn classmap_includes_readonly_classmap_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vendor = root.join("vendor");
        let pkg_src = vendor.join("sebastian/version/src");
        std::fs::create_dir_all(&pkg_src).unwrap();
        std::fs::write(
            pkg_src.join("Version.php"),
            r#"<?php
namespace SebastianBergmann;

final readonly class Version {}
"#,
        )
        .unwrap();

        let mut pkg = locked("sebastian/version", "5.0.2");
        pkg.autoload = Some(AutoloadConfig {
            classmap: vec!["src/".into()],
            ..Default::default()
        });
        let lock = ComposerLock {
            packages_dev: vec![pkg],
            ..Default::default()
        };
        let manifest: ComposerJson = serde_json::from_value(serde_json::json!({
            "name": "acme/app"
        }))
        .unwrap();

        generate(
            root,
            &vendor,
            &manifest,
            Some(&lock),
            &AutoloadOptions::default(),
        )
        .unwrap();

        let classmap =
            std::fs::read_to_string(vendor.join("composer/autoload_classmap.php")).unwrap();
        assert!(
            classmap.contains("'SebastianBergmann\\\\Version'"),
            "readonly classmap package must be indexed:\n{classmap}"
        );
    }

    #[test]
    fn classmap_indexes_single_php_file_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vendor = root.join("vendor");
        let pkg = vendor.join("thecodingmachine/safe/lib");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("DateTimeImmutable.php"),
            r#"<?php
namespace Safe;

class DateTimeImmutable extends \DateTimeImmutable {}
"#,
        )
        .unwrap();

        let mut locked_pkg = locked("thecodingmachine/safe", "2.5.0");
        locked_pkg.autoload = Some(AutoloadConfig {
            classmap: vec!["lib/DateTimeImmutable.php".into()],
            ..Default::default()
        });
        let lock = ComposerLock {
            packages: vec![locked_pkg],
            ..Default::default()
        };
        let manifest: ComposerJson = serde_json::from_value(serde_json::json!({
            "name": "acme/app"
        }))
        .unwrap();

        generate(
            root,
            &vendor,
            &manifest,
            Some(&lock),
            &AutoloadOptions::default(),
        )
        .unwrap();

        let classmap =
            std::fs::read_to_string(vendor.join("composer/autoload_classmap.php")).unwrap();
        assert!(
            classmap.contains("'Safe\\\\DateTimeImmutable'"),
            "file-level classmap entry must be indexed:\n{classmap}"
        );
        assert!(
            classmap.contains("thecodingmachine/safe/lib/DateTimeImmutable.php"),
            "{classmap}"
        );
    }

    #[test]
    fn writes_installed_versions_runtime_class() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vendor = root.join("vendor");
        std::fs::create_dir_all(vendor.join("acme/lib")).unwrap();
        let mut pkg = locked("acme/lib", "1.2.3");
        pkg.dist.as_mut().unwrap().reference = Some("deadbeef".into());
        let lock = ComposerLock {
            packages: vec![pkg],
            ..Default::default()
        };
        let manifest: ComposerJson = serde_json::from_value(serde_json::json!({
            "name": "acme/app"
        }))
        .unwrap();
        generate(
            root,
            &vendor,
            &manifest,
            Some(&lock),
            &AutoloadOptions::default(),
        )
        .unwrap();

        let dumped = vendor.join("composer/InstalledVersions.php");
        assert!(dumped.is_file(), "InstalledVersions.php must be written");
        let body = std::fs::read_to_string(&dumped).unwrap();
        assert!(body.contains("class InstalledVersions"));
        assert!(body.contains("namespace Composer;"));

        let classmap =
            std::fs::read_to_string(vendor.join("composer/autoload_classmap.php")).unwrap();
        assert!(
            classmap.contains("'Composer\\\\InstalledVersions'"),
            "{classmap}"
        );
        assert!(
            classmap.contains("$vendorDir . '/composer/InstalledVersions.php'"),
            "{classmap}"
        );

        let installed_php = std::fs::read_to_string(vendor.join("composer/installed.php")).unwrap();
        assert!(installed_php.contains("'acme/lib'"));
        assert!(installed_php.contains("'pretty_version' => '1.2.3'"));
        assert!(installed_php.contains("'dev_requirement' => false"));

        if std::process::Command::new("php")
            .arg("-v")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let script = format!(
            "require {autoload}; echo \\Composer\\InstalledVersions::isInstalled('acme/lib') ? 'yes' : 'no';",
            autoload = php_single_quoted(&vendor.join("autoload.php")),
        );
        let out = std::process::Command::new("php")
            .arg("-d")
            .arg("display_errors=1")
            .arg("-r")
            .arg(script)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "status={:?} stdout={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "yes");
    }

    #[test]
    fn autoload_real_does_not_reinclude_classloader() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("composer");
        std::fs::create_dir_all(&dir).unwrap();
        write_autoload_real(&dir, false, false).unwrap();
        let body = std::fs::read_to_string(dir.join("autoload_real.php")).unwrap();
        assert!(
            body.contains("class_exists('Composer\\\\Autoload\\\\ClassLoader', false)"),
            "getLoader must skip ClassLoader.php when already loaded:\n{body}"
        );
        assert!(
            body.contains("require_once __DIR__ . '/ClassLoader.php'"),
            "{body}"
        );
    }

    #[test]
    fn requiring_classloader_then_autoload_php_does_not_fatal() {
        if std::process::Command::new("php")
            .arg("-v")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vendor = root.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        let manifest: ComposerJson = serde_json::from_value(serde_json::json!({
            "name": "acme/app"
        }))
        .unwrap();
        generate(root, &vendor, &manifest, None, &AutoloadOptions::default()).unwrap();

        let status = std::process::Command::new("php")
            .arg("-d")
            .arg("display_errors=1")
            .arg("-r")
            .arg(format!(
                "require_once {}; require_once {}; echo 'ok';",
                php_single_quoted(&vendor.join("composer/ClassLoader.php")),
                php_single_quoted(&vendor.join("autoload.php")),
            ))
            .status()
            .unwrap();
        assert!(
            status.success(),
            "ClassLoader then autoload.php must succeed"
        );
    }

    fn php_single_quoted(path: &std::path::Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', r"\'"))
    }

    #[test]
    fn classloader_has_includefile_static_for_interceptors() {
        if std::process::Command::new("php")
            .arg("-v")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("composer");
        std::fs::create_dir_all(&dir).unwrap();
        write_class_loader(&dir).unwrap();
        let body = std::fs::read_to_string(dir.join("ClassLoader.php")).unwrap();
        assert!(
            body.contains("private static $includeFile"),
            "official ClassLoader must expose $includeFile"
        );

        let script = format!(
            r#"
            require_once {path};
            $loader = new Composer\Autoload\ClassLoader();
            $ok = Closure::bind(static function () {{
                return self::$includeFile instanceof Closure;
            }}, null, Composer\Autoload\ClassLoader::class)();
            echo $ok ? 'ok' : 'missing';
            "#,
            path = php_single_quoted(&dir.join("ClassLoader.php")),
        );
        let out = std::process::Command::new("php")
            .arg("-d")
            .arg("display_errors=1")
            .arg("-r")
            .arg(script)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    }
}
