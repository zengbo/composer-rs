//! PubGrub-based dependency resolver for Composer packages.
//!
//! Flow:
//! 1. Load path / VCS / Packagist package sources
//! 2. Prefetch package metadata in parallel waves
//! 3. Build an offline PubGrub provider and solve
//! 4. Map solution → locked packages

#![deny(unsafe_code)]

mod index;
mod provider;
mod sources;

pub use index::PackageIndex;
pub use sources::{LocalPathPackage, SourceKind};

use composer_core::error::{Error, Result};
use composer_core::{PackageId, Platform, VersionConstraint};
use composer_lock::{ComposerLock, LockedPackage};
use composer_manifest::{ComposerJson, Repository};
use composer_repo::RepositoryRegistry;
use provider::{normalize_solution, solve_with_pubgrub, SolveRequest};
use sources::collect_local_sources;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use tracing::info;

/// Resolver configuration.
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    pub with_dev: bool,
    pub prefer_stable: bool,
    pub prefer_lowest: bool,
    pub minimum_stability: String,
    pub concurrency: usize,
    pub ignore_platform_reqs: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            with_dev: true,
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: "stable".into(),
            concurrency: 32,
            ignore_platform_reqs: false,
        }
    }
}

/// Resolution result ready to write as composer.lock and install.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub packages: Vec<LockedPackage>,
    pub packages_dev: Vec<LockedPackage>,
}

impl Resolution {
    pub fn to_lock(&self, manifest: &ComposerJson) -> ComposerLock {
        let mut lock = ComposerLock::default();
        lock.packages = self.packages.clone();
        lock.packages_dev = self.packages_dev.clone();
        lock.minimum_stability = manifest.minimum_stability().to_string();
        lock.prefer_stable = manifest.prefer_stable();
        lock.content_hash = composer_lock::content_hash_from_relevant(
            &composer_manifest::relevant_content(manifest),
        );
        for (k, v) in &manifest.require {
            let id = PackageId::parse(k).ok();
            if id.as_ref().is_some_and(|p| p.is_platform()) {
                lock.platform.insert(k.clone(), v.clone());
            }
        }
        for (k, v) in &manifest.require_dev {
            let id = PackageId::parse(k).ok();
            if id.as_ref().is_some_and(|p| p.is_platform()) {
                lock.platform_dev.insert(k.clone(), v.clone());
            }
        }
        lock
    }

    pub fn all_packages(&self) -> impl Iterator<Item = &LockedPackage> {
        self.packages.iter().chain(self.packages_dev.iter())
    }
}

/// Resolve dependencies for a project manifest.
pub async fn resolve(
    manifest: &ComposerJson,
    options: &ResolveOptions,
    project_root: &Path,
) -> Result<Resolution> {
    let root_prod = manifest.prod_deps()?;
    let root_dev = if options.with_dev {
        manifest.dev_deps()?
    } else {
        Vec::new()
    };
    let dev_roots: BTreeSet<String> = root_dev.iter().map(|(id, _)| id.to_string()).collect();

    let mut all_roots = root_prod.clone();
    all_roots.extend(root_dev);

    // Local path + VCS package definitions take priority over Packagist.
    let repos = manifest.repositories_list();
    let local = collect_local_sources(project_root, &repos)?;
    info!(
        path_packages = local.path_packages.len(),
        vcs_packages = local.vcs_packages.len(),
        "loaded custom repositories"
    );

    let mut index = PackageIndex::new();
    for pkg in &local.path_packages {
        index.insert_local(pkg.clone());
    }
    for pkg in &local.vcs_packages {
        index.insert_local(pkg.clone());
    }

    // Inline "package" repositories
    for repo in &repos {
        if let Repository::Package { packages } = repo {
            for p in packages {
                if let Ok(locked) = serde_json::from_value::<LockedPackage>(p.clone()) {
                    index.insert_locked(locked);
                }
            }
        }
    }

    // Prefetch remote metadata for the dependency closure.
    let registry = RepositoryRegistry::from_manifest(manifest)?;
    prefetch_remote(&registry, &mut index, &all_roots, options).await?;

    index.register_virtual_packages();

    let mut platform = Platform::detect()?;
    platform.apply_config_platform(manifest.config.as_ref());
    if !platform.reliable && !options.ignore_platform_reqs {
        tracing::warn!(
            "PHP not detected; platform filtering disabled during resolve \
             (set COMPOSER_PLATFORM_PHP, config.platform, or --ignore-platform-reqs)"
        );
    }

    let request = SolveRequest {
        root_deps: all_roots,
        prefer_stable: options.prefer_stable,
        prefer_lowest: options.prefer_lowest,
        minimum_stability: composer_core::Stability::parse(&options.minimum_stability),
        root_replace: manifest.replace.keys().cloned().collect(),
        root_provide: manifest.provide.keys().cloned().collect(),
        platform,
        ignore_platform_reqs: options.ignore_platform_reqs,
    };

    let selected = normalize_solution(solve_with_pubgrub(&index, &request)?, &index)?;

    // Split prod vs dev
    let mut prod_names: BTreeSet<String> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<String> = root_prod
        .into_iter()
        .map(|(id, _)| id.to_string())
        .collect();
    while let Some(name) = queue.pop_front() {
        if !prod_names.insert(name.clone()) {
            continue;
        }
        if let Some(pkg) = selected.get(&name) {
            for dep in pkg.require.keys() {
                let id = PackageId::parse(dep).ok();
                if id.as_ref().is_some_and(|p| !p.is_platform()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    let mut packages = Vec::new();
    let mut packages_dev = Vec::new();
    let mut names: Vec<_> = selected.keys().cloned().collect();
    names.sort();
    for name in names {
        let pkg = selected[&name].clone();
        if prod_names.contains(&name) {
            packages.push(pkg);
        } else if dev_roots.contains(&name) || !prod_names.contains(&name) {
            packages_dev.push(pkg);
        }
    }
    packages_dev.retain(|p| !packages.iter().any(|x| x.name == p.name));

    Ok(Resolution {
        packages,
        packages_dev,
    })
}

async fn prefetch_remote(
    registry: &RepositoryRegistry,
    index: &mut PackageIndex,
    roots: &[(PackageId, VersionConstraint)],
    options: &ResolveOptions,
) -> Result<()> {
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::collections::VecDeque;
    use std::sync::Arc;

    let mut queue: VecDeque<String> = roots.iter().map(|(id, _)| id.to_string()).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    while !queue.is_empty() {
        let mut wave = Vec::new();
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name.clone()) {
                continue;
            }
            // Already provided by path/vcs/inline
            if index.has_package(&name) {
                // Still walk its deps
                if let Some(versions) = index.versions_of(&name) {
                    for v in versions {
                        for dep in v.require.keys() {
                            let id = PackageId::parse(dep).ok();
                            if id.as_ref().is_some_and(|p| !p.is_platform())
                                && !seen.contains(dep)
                            {
                                queue.push_back(dep.clone());
                            }
                        }
                    }
                }
                continue;
            }
            if PackageId::parse(&name).is_ok_and(|p| p.is_platform()) {
                continue;
            }
            wave.push(name);
        }

        if wave.is_empty() {
            continue;
        }

        info!(count = wave.len(), "prefetching package metadata");
        let sem = Arc::new(tokio::sync::Semaphore::new(options.concurrency));
        let mut futs = FuturesUnordered::new();
        for name in wave {
            let registry = registry.clone();
            let sem = Arc::clone(&sem);
            futs.push(async move {
                let _p = sem.acquire().await.ok();
                let id = PackageId::parse(&name)?;
                let versions = registry.get_package_versions(&id).await?;
                Ok::<_, Error>((name, versions))
            });
        }

        while let Some(res) = futs.next().await {
            let (name, versions) = res?;
            for v in &versions {
                for dep in v.require.keys() {
                    let id = PackageId::parse(dep).ok();
                    if id.as_ref().is_some_and(|p| !p.is_platform()) && !seen.contains(dep) {
                        queue.push_back(dep.clone());
                    }
                }
            }
            index.insert_remote(&name, versions);
        }
    }

    Ok(())
}

/// Build a map of package name → locked package from an existing lock.
pub fn locked_map(lock: &ComposerLock, with_dev: bool) -> HashMap<String, LockedPackage> {
    let mut map = HashMap::new();
    for p in &lock.packages {
        map.insert(p.name.clone(), p.clone());
    }
    if with_dev {
        for p in &lock.packages_dev {
            map.insert(p.name.clone(), p.clone());
        }
    }
    map
}

/// Collect locked packages sorted by name.
pub fn locked_list(lock: &ComposerLock, with_dev: bool) -> Vec<LockedPackage> {
    let mut pkgs: Vec<_> = lock
        .packages_to_install(with_dev)
        .into_iter()
        .cloned()
        .collect();
    pkgs.sort_by(|a, b| a.name.cmp(&b.name));
    pkgs
}
