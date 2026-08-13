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

use composer_auth::AuthStore;
use composer_core::error::{Error, Result};
use composer_core::{PackageId, Platform, VersionConstraint};
use composer_lock::{ComposerLock, LockedPackage};
use composer_manifest::{ComposerJson, Repository};
use composer_repo::RepositoryRegistry;
use provider::{SolveRequest, normalize_solution, solve_with_pubgrub};
use sources::collect_local_sources;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use tracing::info;

/// How far a partial update may walk dependency edges (Composer `-w` / `-W`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdateDeps {
    /// Only packages named on the command line (Composer default).
    #[default]
    OnlyListed,
    /// Also update dependencies of listed packages, except root requirements (`-w`).
    WithDependencies,
    /// Also update dependencies of listed packages, including root requirements (`-W`).
    WithAllDependencies,
}

/// Resolver configuration.
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    pub with_dev: bool,
    pub prefer_stable: bool,
    pub prefer_lowest: bool,
    pub minimum_stability: String,
    pub concurrency: usize,
    pub ignore_platform_reqs: bool,
    /// Per-requirement ignore patterns (e.g. `ext-xdebug`, `ext-*`).
    pub ignore_platform_req: Vec<String>,
    /// When non-empty, only these packages (see [`UpdateDeps`]) may change version;
    /// other locked packages stay pinned. Requires `existing_lock` in `resolve`.
    pub packages_to_update: Vec<String>,
    /// Scope of partial updates when `packages_to_update` is non-empty.
    pub update_deps: UpdateDeps,
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
            ignore_platform_req: Vec::new(),
            packages_to_update: Vec::new(),
            update_deps: UpdateDeps::OnlyListed,
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
    pub fn to_lock(&self, manifest: &ComposerJson, composer_json_bytes: &[u8]) -> ComposerLock {
        let mut lock = ComposerLock::default();
        lock.packages = self.packages.clone();
        lock.packages_dev = self.packages_dev.clone();
        lock.minimum_stability = manifest.minimum_stability().to_string();
        lock.prefer_stable = manifest.prefer_stable();
        lock.content_hash = composer_lock::content_hash_from_composer_json(composer_json_bytes)
            .expect("content_hash: composer.json already validated");
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
///
/// Pass `existing_lock` when updating an existing project. If `options.packages_to_update`
/// is non-empty, packages outside the update whitelist keep their locked versions.
/// Whitelist scope is controlled by [`ResolveOptions::update_deps`] (`-w` / `-W`).
pub async fn resolve(
    manifest: &ComposerJson,
    options: &ResolveOptions,
    project_root: &Path,
    existing_lock: Option<&ComposerLock>,
) -> Result<Resolution> {
    let root_prod = manifest.prod_deps()?;
    let root_dev = if options.with_dev {
        manifest.dev_deps()?
    } else {
        Vec::new()
    };
    let dev_roots: BTreeSet<String> = root_dev.iter().map(|(id, _)| id.to_string()).collect();
    let root_req_names = root_requirement_names(manifest, options.with_dev);

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
    // Load project-local auth.json so private Composer repos work during resolve/require.
    let auth = AuthStore::load(Some(project_root)).unwrap_or_default();
    let registry = RepositoryRegistry::from_manifest_auth(manifest, auth)?;
    let missing = prefetch_remote(&registry, &mut index, &all_roots, options).await?;

    if let Some(lock) = existing_lock {
        if !options.packages_to_update.is_empty() {
            let mutable = packages_allowed_to_change(
                &options.packages_to_update,
                lock,
                options.with_dev,
                options.update_deps,
                &root_req_names,
            );
            let pins = build_locked_pins(lock, &mutable, options.with_dev);
            info!(
                pinned = pins.len(),
                mutable = mutable.len(),
                ?options.update_deps,
                "partial update: pinning unchanged packages"
            );
            index.pin_to_locked(&pins);
        }
    }

    index.register_virtual_packages();
    for name in &missing {
        if !index.has_package(name) {
            return Err(Error::PackageNotFound(format!(
                "{name} (not on the repository and no installed package provides it)"
            )));
        }
    }

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
        ignore_platform_req: options.ignore_platform_req.clone(),
    };

    let selected = normalize_solution(solve_with_pubgrub(&index, &request)?, &index)?;

    // Split prod vs dev. Virtual root requirements (`provide`) map onto the
    // real provider after normalize_solution, so seed those providers as prod.
    let mut prod_names: BTreeSet<String> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for (id, _) in &root_prod {
        let name = id.to_string();
        queue.push_back(name.clone());
        for (sel_name, pkg) in &selected {
            if pkg.provide.contains_key(&name) || pkg.replace.contains_key(&name) {
                queue.push_back(sel_name.clone());
            }
        }
    }
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

enum PrefetchHit {
    Found {
        name: String,
        versions: Vec<composer_repo::RemotePackageVersion>,
    },
    /// Packagist/GitLab 404 — may still be a `provide` / `replace` virtual.
    Missing(String),
}

async fn prefetch_remote(
    registry: &RepositoryRegistry,
    index: &mut PackageIndex,
    roots: &[(PackageId, VersionConstraint)],
    options: &ResolveOptions,
) -> Result<BTreeSet<String>> {
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::collections::VecDeque;
    use std::sync::Arc;

    let mut queue: VecDeque<String> = roots.iter().map(|(id, _)| id.to_string()).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut missing = BTreeSet::new();

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
                            if id.as_ref().is_some_and(|p| !p.is_platform()) && !seen.contains(dep)
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
                match registry.get_package_versions(&id).await {
                    Ok(versions) => Ok(PrefetchHit::Found { name, versions }),
                    Err(Error::PackageNotFound(_)) => Ok(PrefetchHit::Missing(name)),
                    Err(e) => Err(e),
                }
            });
        }

        while let Some(res) = futs.next().await {
            match res? {
                PrefetchHit::Missing(name) => {
                    missing.insert(name);
                }
                PrefetchHit::Found { name, versions } => {
                    for v in &versions {
                        for virt in v.provide.keys().chain(v.replace.keys()) {
                            seen.insert(virt.clone());
                            missing.remove(virt);
                        }
                        for dep in v.require.keys() {
                            let id = PackageId::parse(dep).ok();
                            if id.as_ref().is_some_and(|p| !p.is_platform()) && !seen.contains(dep)
                            {
                                queue.push_back(dep.clone());
                            }
                        }
                    }
                    index.insert_remote(&name, versions);
                }
            }
        }
    }

    Ok(missing)
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

#[cfg(test)]
mod partial_update_tests {
    use super::*;
    use composer_lock::{DistInfo, LockedPackage};
    use std::collections::BTreeMap;

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
                url: format!("https://example.com/{name}.zip"),
                reference: None,
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

    #[test]
    fn only_listed_does_not_walk_deps() {
        let lock = ComposerLock {
            packages: vec![
                locked("vendor/a", "1.0.0", &[("vendor/c", "^1.0")]),
                locked("vendor/b", "1.0.0", &[]),
                locked("vendor/c", "1.0.0", &[]),
            ],
            ..Default::default()
        };
        let roots = BTreeSet::from(["vendor/a".into(), "vendor/b".into()]);
        let mutable = packages_allowed_to_change(
            &["vendor/a".into()],
            &lock,
            true,
            UpdateDeps::OnlyListed,
            &roots,
        );
        assert!(mutable.contains("vendor/a"));
        assert!(!mutable.contains("vendor/c"));
        assert!(!mutable.contains("vendor/b"));
    }

    #[test]
    fn with_dependencies_walks_non_root_deps() {
        let lock = ComposerLock {
            packages: vec![
                locked("vendor/a", "1.0.0", &[("vendor/c", "^1.0")]),
                locked("vendor/b", "1.0.0", &[]),
                locked("vendor/c", "1.0.0", &[]),
            ],
            ..Default::default()
        };
        // vendor/c is transitive only (not a root requirement)
        let roots = BTreeSet::from(["vendor/a".into(), "vendor/b".into()]);
        let mutable = packages_allowed_to_change(
            &["vendor/a".into()],
            &lock,
            true,
            UpdateDeps::WithDependencies,
            &roots,
        );
        assert!(mutable.contains("vendor/a"));
        assert!(mutable.contains("vendor/c"));
        assert!(!mutable.contains("vendor/b"));
    }

    #[test]
    fn with_dependencies_skips_root_requirements() {
        let lock = ComposerLock {
            packages: vec![
                locked("vendor/a", "1.0.0", &[("vendor/b", "^1.0")]),
                locked("vendor/b", "1.0.0", &[]),
            ],
            ..Default::default()
        };
        // vendor/b is also a root requirement → -w must not free it
        let roots = BTreeSet::from(["vendor/a".into(), "vendor/b".into()]);
        let mutable = packages_allowed_to_change(
            &["vendor/a".into()],
            &lock,
            true,
            UpdateDeps::WithDependencies,
            &roots,
        );
        assert!(mutable.contains("vendor/a"));
        assert!(!mutable.contains("vendor/b"));
    }

    #[test]
    fn with_all_dependencies_includes_root_requirements() {
        let lock = ComposerLock {
            packages: vec![
                locked("vendor/a", "1.0.0", &[("vendor/b", "^1.0")]),
                locked("vendor/b", "1.0.0", &[]),
            ],
            ..Default::default()
        };
        let roots = BTreeSet::from(["vendor/a".into(), "vendor/b".into()]);
        let mutable = packages_allowed_to_change(
            &["vendor/a".into()],
            &lock,
            true,
            UpdateDeps::WithAllDependencies,
            &roots,
        );
        assert!(mutable.contains("vendor/a"));
        assert!(mutable.contains("vendor/b"));
    }

    #[test]
    fn with_dependencies_walks_require_dev_when_with_dev() {
        let lock = ComposerLock {
            packages: vec![locked_full(
                "vendor/a",
                "1.0.0",
                &[],
                &[("vendor/phpunit", "^10.0")],
            )],
            packages_dev: vec![locked("vendor/phpunit", "10.0.0", &[])],
            ..Default::default()
        };
        let roots = BTreeSet::from(["vendor/a".into(), "vendor/phpunit".into()]);
        // vendor/phpunit is a root require-dev → -w still skips root requirements
        let mutable_w = packages_allowed_to_change(
            &["vendor/a".into()],
            &lock,
            true,
            UpdateDeps::WithDependencies,
            &roots,
        );
        assert!(mutable_w.contains("vendor/a"));
        assert!(!mutable_w.contains("vendor/phpunit"));

        // -W frees root require-dev via require-dev edge
        let mutable_w_all = packages_allowed_to_change(
            &["vendor/a".into()],
            &lock,
            true,
            UpdateDeps::WithAllDependencies,
            &roots,
        );
        assert!(mutable_w_all.contains("vendor/phpunit"));
    }

    #[test]
    fn with_dependencies_skips_require_dev_when_no_dev() {
        let lock = ComposerLock {
            packages: vec![locked_full(
                "vendor/a",
                "1.0.0",
                &[("vendor/c", "^1.0")],
                &[("vendor/devtool", "^1.0")],
            )],
            packages_dev: vec![locked("vendor/devtool", "1.0.0", &[])],
            ..Default::default()
        };
        // vendor/c is not a root req; vendor/devtool is only require-dev
        let roots = BTreeSet::from(["vendor/a".into()]);
        let mutable = packages_allowed_to_change(
            &["vendor/a".into()],
            &lock,
            false, // --no-dev
            UpdateDeps::WithAllDependencies,
            &roots,
        );
        assert!(mutable.contains("vendor/a"));
        assert!(mutable.contains("vendor/c"));
        assert!(!mutable.contains("vendor/devtool"));
    }

    #[test]
    fn with_w_frees_non_root_require_dev_dep() {
        // require-dev dep that is NOT a root requirement (edge case in lock metadata)
        let lock = ComposerLock {
            packages: vec![
                locked_full("vendor/a", "1.0.0", &[], &[("vendor/test-util", "^1.0")]),
                locked("vendor/test-util", "1.0.0", &[]),
            ],
            ..Default::default()
        };
        let roots = BTreeSet::from(["vendor/a".into()]);
        let mutable = packages_allowed_to_change(
            &["vendor/a".into()],
            &lock,
            true,
            UpdateDeps::WithDependencies,
            &roots,
        );
        assert!(mutable.contains("vendor/a"));
        assert!(mutable.contains("vendor/test-util"));
    }

    #[test]
    fn pins_exclude_mutable_packages() {
        let lock = ComposerLock {
            packages: vec![
                locked("vendor/a", "1.0.0", &[]),
                locked("vendor/b", "2.0.0", &[]),
            ],
            ..Default::default()
        };
        let mutable = BTreeSet::from(["vendor/a".into()]);
        let pins = build_locked_pins(&lock, &mutable, true);
        assert_eq!(
            pins.get("vendor/b").map(|p| p.version.as_str()),
            Some("2.0.0")
        );
        assert!(!pins.contains_key("vendor/a"));
    }

    #[test]
    fn pin_to_locked_injects_missing_version() {
        let mut index = PackageIndex::new();
        // Registry only has 1.1.0; lock pins 1.0.0.
        index.insert_locked(locked("vendor/b", "1.1.0", &[]));
        let mut pins = HashMap::new();
        pins.insert("vendor/b".into(), locked("vendor/b", "1.0.0", &[]));
        index.pin_to_locked(&pins);
        let versions: Vec<_> = index
            .all_versions("vendor/b")
            .into_iter()
            .map(|iv| iv.locked.version.as_str())
            .collect();
        assert_eq!(versions, vec!["1.0.0"]);
    }
}

/// Non-platform package names listed in the root manifest's require / require-dev.
pub fn root_requirement_names(manifest: &ComposerJson, with_dev: bool) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for key in manifest.require.keys() {
        if PackageId::parse(key).is_ok_and(|p| !p.is_platform()) {
            names.insert(key.clone());
        }
    }
    if with_dev {
        for key in manifest.require_dev.keys() {
            if PackageId::parse(key).is_ok_and(|p| !p.is_platform()) {
                names.insert(key.clone());
            }
        }
    }
    names
}

/// Packages whose versions may change during a partial update.
///
/// Matches Composer semantics:
/// - [`UpdateDeps::OnlyListed`]: only `targets`
/// - [`UpdateDeps::WithDependencies`] (`-w`): targets + transitive deps, excluding
///   packages that are also root requirements
/// - [`UpdateDeps::WithAllDependencies`] (`-W`): targets + all transitive deps
///
/// When `with_dev` is true, dependency edges from each package's `require-dev` are
/// walked as well (Composer includes them for root / `--dev` updates).
pub fn packages_allowed_to_change(
    targets: &[String],
    lock: &ComposerLock,
    with_dev: bool,
    mode: UpdateDeps,
    root_requirements: &BTreeSet<String>,
) -> BTreeSet<String> {
    let map = locked_map(lock, with_dev);
    let mut mutable: BTreeSet<String> = targets.iter().cloned().collect();

    if matches!(mode, UpdateDeps::OnlyListed) {
        return mutable;
    }

    let mut queue: std::collections::VecDeque<String> = targets.iter().cloned().collect();
    while let Some(name) = queue.pop_front() {
        let Some(pkg) = map.get(&name) else {
            continue;
        };
        for dep in package_dep_names(pkg, with_dev) {
            if !PackageId::parse(dep).is_ok_and(|p| !p.is_platform()) {
                continue;
            }
            if matches!(mode, UpdateDeps::WithDependencies) && root_requirements.contains(dep) {
                continue;
            }
            if mutable.insert(dep.clone()) {
                queue.push_back(dep.clone());
            }
        }
    }
    mutable
}

/// Production requires, plus `require-dev` when installing with dev dependencies.
fn package_dep_names(pkg: &LockedPackage, with_dev: bool) -> impl Iterator<Item = &String> {
    let prod = pkg.require.keys();
    let dev = pkg.require_dev.keys();
    prod.chain(dev.filter(move |_| with_dev))
}

fn build_locked_pins(
    lock: &ComposerLock,
    mutable: &BTreeSet<String>,
    with_dev: bool,
) -> HashMap<String, LockedPackage> {
    let mut pins = HashMap::new();
    for pkg in lock.packages_to_install(with_dev) {
        if !mutable.contains(&pkg.name) {
            pins.insert(pkg.name.clone(), pkg.clone());
        }
    }
    pins
}
