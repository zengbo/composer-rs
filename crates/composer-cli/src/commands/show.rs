//! `composer-rs show`

use super::{header, project_paths, success};
use anyhow::{Result, bail};
use clap::Args;
use composer_auth::AuthStore;
use composer_core::PackageId;
use composer_lock::{ComposerLock, LockedPackage};
use composer_manifest::ComposerJson;
use composer_repo::RepositoryRegistry;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Args, Debug, Clone)]
pub struct ShowArgs {
    /// Package name (omit to list installed)
    pub package: Option<String>,

    /// Show all available versions from Packagist
    #[arg(long)]
    pub all: bool,

    /// Print dependency tree from lock
    #[arg(long, short = 't')]
    pub tree: bool,

    /// Only direct (root) requirements when listing
    #[arg(long, short = 'D')]
    pub direct: bool,

    #[arg(long)]
    pub no_dev: bool,

    /// Show installed path
    #[arg(long, short = 'P')]
    pub path: bool,
}

pub async fn run(args: ShowArgs) -> Result<()> {
    let (cwd, json_path, lock_path) = project_paths()?;

    if args.tree {
        return show_tree(
            &lock_path,
            &json_path,
            !args.no_dev,
            args.package.as_deref(),
        );
    }

    if let Some(name) = &args.package {
        // Prefer lock details when installed
        if lock_path.exists() {
            let lock = ComposerLock::load(&lock_path)?;
            if let Some(pkg) = lock.find(name) {
                print_locked(pkg, args.path, &cwd, &json_path)?;
                if !args.all {
                    success("done");
                    return Ok(());
                }
            }
        }

        header(&format!("Package {name}"));
        let id = PackageId::parse(name)?;
        let auth = AuthStore::load(Some(&cwd)).unwrap_or_default();
        let versions = if json_path.exists() {
            let manifest = ComposerJson::load(&json_path)?;
            let registry = RepositoryRegistry::from_manifest_auth(&manifest, auth)?;
            registry.show(&id).await?
        } else {
            let client = composer_repo::RepositoryClient::with_base_url_auth(
                "https://repo.packagist.org",
                auth,
            )?;
            client.show(&id).await?
        };
        if versions.is_empty() {
            bail!("package not found");
        }

        if args.all {
            for v in &versions {
                println!("  {}", v.version.raw);
            }
        } else {
            let latest = versions
                .iter()
                .filter(|v| v.version.is_stable())
                .max_by(|a, b| a.version.cmp(&b.version))
                .or_else(|| versions.first())
                .unwrap();
            println!("name       : {}", latest.name);
            println!("version    : {}", latest.version.raw);
            println!(
                "type       : {}",
                latest.package_type.as_deref().unwrap_or("library")
            );
            if let Some(d) = &latest.description {
                println!("description: {d}");
            }
            if let Some(dist) = &latest.dist {
                println!("dist       : {}", dist.url);
            }
            if !latest.require.is_empty() {
                println!("requires:");
                for (k, v) in &latest.require {
                    println!("  {k}: {v}");
                }
            }
        }
        success("done");
        return Ok(());
    }

    // List installed from lock
    header("Installed packages");
    if !lock_path.exists() {
        bail!("no composer.lock — nothing installed to show");
    }
    let lock = ComposerLock::load(&lock_path)?;
    let with_dev = !args.no_dev;
    let root_direct: BTreeSet<String> = if args.direct && json_path.exists() {
        let m = composer_manifest::ComposerJson::load(&json_path)?;
        let mut s: BTreeSet<String> = m.require.keys().cloned().collect();
        if with_dev {
            s.extend(m.require_dev.keys().cloned());
        }
        s
    } else {
        BTreeSet::new()
    };

    let mut count = 0usize;
    for p in lock.packages_to_install(with_dev) {
        if args.direct && !root_direct.contains(&p.name) {
            continue;
        }
        println!("  {}  {}", p.name, p.version);
        count += 1;
    }
    success(&format!("{count} package(s)"));
    Ok(())
}

fn print_locked(
    pkg: &LockedPackage,
    show_path: bool,
    cwd: &std::path::Path,
    json_path: &std::path::Path,
) -> Result<()> {
    header(&format!("Package {}", pkg.name));
    println!("name       : {}", pkg.name);
    println!("version    : {}", pkg.version);
    println!(
        "type       : {}",
        pkg.package_type.as_deref().unwrap_or("library")
    );
    if let Some(d) = &pkg.description {
        println!("description: {d}");
    }
    if !pkg.require.is_empty() {
        println!("requires:");
        for (k, v) in &pkg.require {
            println!("  {k}: {v}");
        }
    }
    if show_path && json_path.exists() {
        let manifest = composer_manifest::ComposerJson::load(json_path)?;
        let vendor = cwd.join(manifest.vendor_dir());
        let paths = manifest.installer_paths();
        let dest = paths
            .resolve(cwd, &pkg.name, pkg.package_type.as_deref())
            .unwrap_or_else(|| vendor.join(&pkg.name));
        println!("path       : {}", dest.display());
    }
    Ok(())
}

fn show_tree(
    lock_path: &std::path::Path,
    json_path: &std::path::Path,
    with_dev: bool,
    focus: Option<&str>,
) -> Result<()> {
    if !lock_path.exists() {
        bail!("no composer.lock — cannot show tree");
    }
    let lock = ComposerLock::load(lock_path)?;
    let packages: BTreeMap<String, &LockedPackage> = lock
        .packages_to_install(with_dev)
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect();

    let roots: Vec<String> = if let Some(name) = focus {
        vec![name.to_string()]
    } else if json_path.exists() {
        let m = composer_manifest::ComposerJson::load(json_path)?;
        let mut r: Vec<_> = m
            .require
            .keys()
            .filter(|k| PackageId::parse(k).is_ok_and(|p| !p.is_platform()))
            .cloned()
            .collect();
        if with_dev {
            r.extend(
                m.require_dev
                    .keys()
                    .filter(|k| PackageId::parse(k).is_ok_and(|p| !p.is_platform()))
                    .cloned(),
            );
        }
        r.sort();
        r.dedup();
        r
    } else {
        packages.keys().cloned().collect()
    };

    header("Dependency tree");
    for root in &roots {
        let ver = packages
            .get(root)
            .map(|p| p.version.as_str())
            .unwrap_or("?");
        println!("{root} {ver}");
        print_children(root, &packages, with_dev, "", &mut BTreeSet::new(), 0);
    }
    Ok(())
}

fn print_children(
    name: &str,
    packages: &BTreeMap<String, &LockedPackage>,
    with_dev: bool,
    prefix: &str,
    seen: &mut BTreeSet<String>,
    depth: usize,
) {
    if depth > 24 || !seen.insert(name.to_string()) {
        return;
    }
    let Some(pkg) = packages.get(name) else {
        return;
    };
    // Match depends/why: walk require, and require-dev when with_dev is on.
    let mut deps: Vec<(&String, &String, &str)> = pkg
        .require
        .iter()
        .filter(|(k, _)| PackageId::parse(k).is_ok_and(|p| !p.is_platform()))
        .map(|(k, v)| (k, v, "requires"))
        .collect();
    if with_dev {
        for (k, v) in &pkg.require_dev {
            if PackageId::parse(k).is_ok_and(|p| !p.is_platform()) {
                deps.push((k, v, "require-dev"));
            }
        }
    }
    deps.sort_by(|a, b| a.0.cmp(b.0));

    for (i, (dep, constraint, kind)) in deps.iter().enumerate() {
        let last = i + 1 == deps.len();
        let branch = if last { "└──" } else { "├──" };
        let ver = packages
            .get(*dep)
            .map(|p| p.version.as_str())
            .unwrap_or("?");
        println!("{prefix}{branch} {dep} {ver} ({kind} {constraint})");
        let next = format!("{prefix}{}", if last { "    " } else { "│   " });
        print_children(dep, packages, with_dev, &next, seen, depth + 1);
    }
}
