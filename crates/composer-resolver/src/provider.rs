//! PubGrub dependency provider over the package index.

use crate::index::PackageIndex;
use composer_core::error::{Error, Result};
use composer_core::{
    ComposerVersion, PackageId, Platform, Stability, VersionConstraint,
    check_requirements_filtered, constraint_to_ranges,
};
use composer_lock::LockedPackage;
use pubgrub::{
    Dependencies, DependencyConstraints, DependencyProvider, OfflineDependencyProvider,
    PackageResolutionStatistics, Ranges, resolve,
};
use std::collections::HashMap;
use std::convert::Infallible;
use tracing::debug;

const ROOT: &str = "__root__/__root__";
const CONFLICT_PREFIX: &str = "__composer_rs_conflict__/";

#[derive(Debug, Clone)]
pub struct SolveRequest {
    pub root_deps: Vec<(PackageId, VersionConstraint)>,
    pub prefer_stable: bool,
    pub prefer_lowest: bool,
    pub minimum_stability: Stability,
    pub root_replace: Vec<String>,
    pub root_provide: Vec<String>,
    pub root_conflicts: Vec<(PackageId, VersionConstraint)>,
    pub platform: Platform,
    pub ignore_platform_reqs: bool,
    pub ignore_platform_req: Vec<String>,
}

/// Run PubGrub and return selected locked packages by name.
pub fn solve_with_pubgrub(
    index: &PackageIndex,
    request: &SolveRequest,
) -> Result<HashMap<String, LockedPackage>> {
    // Discrete approach: only register versions that exist in the index.
    // Constraints are encoded as Ranges over ComposerVersion; choose_version
    // filters with range.contains.
    //
    // We use OfflineDependencyProvider with String packages and Ranges<ComposerVersion>.
    // For prefer_lowest, we insert versions in reverse order so rev() picks lowest...
    // OfflineDependencyProvider always picks highest (rev on BTreeMap keys).
    // So for prefer_lowest we wrap with a custom provider.

    let provider = ComposerOfflineProvider::build(index, request)?;
    let root_version = ComposerVersion::parse("1.0.0").unwrap();

    let solution = resolve(&provider, ROOT.to_string(), root_version)
        .map_err(|e| Error::Resolve(format!("PubGrub could not find a set of packages: {e}")))?;

    let mut out = HashMap::new();
    for (name, version) in solution {
        if name == ROOT || name.starts_with(CONFLICT_PREFIX) {
            continue;
        }
        let raw = version.raw.clone();
        if let Some(idx) = index.get(&name, &raw) {
            out.insert(name, idx.locked.clone());
        } else {
            // Try match by version equality across all
            if let Some(v) = index
                .all_versions(&name)
                .into_iter()
                .find(|v| v.version == version)
            {
                out.insert(name, v.locked.clone());
            } else {
                return Err(Error::Resolve(format!(
                    "solution references unknown {name}@{raw}"
                )));
            }
        }
    }
    validate_conflicts(&out)?;
    validate_root_conflicts(&out, &request.root_conflicts)?;
    Ok(out)
}

/// Remap virtual packages to real providers and drop replaced packages.
pub fn normalize_solution(
    selected: HashMap<String, LockedPackage>,
    index: &PackageIndex,
) -> Result<HashMap<String, LockedPackage>> {
    let mut remapped = HashMap::new();
    for (name, pkg) in selected {
        if name == ROOT {
            continue;
        }
        let real = index
            .real_provider_for(&name, &pkg.version)
            .unwrap_or_else(|| index.real_package_name(&name));
        let locked = if real != name {
            index
                .provider_locked(real, &pkg.version)
                .unwrap_or_else(|| {
                    let mut p = pkg;
                    p.name = real.to_string();
                    p
                })
        } else {
            pkg
        };
        // Prefer keeping the first real package if two virtuals collapse to one.
        remapped.entry(real.to_string()).or_insert(locked);
    }

    let mut to_remove = Vec::new();
    for (name, pkg) in &remapped {
        for replaced in pkg.replace.keys() {
            if remapped.contains_key(replaced.as_str()) && replaced != name {
                to_remove.push(replaced.clone());
            }
        }
    }
    for name in to_remove {
        remapped.remove(&name);
    }

    Ok(remapped)
}

fn version_matches_platform(
    locked: &LockedPackage,
    platform: &Platform,
    ignore_all: bool,
    ignore_patterns: &[String],
) -> bool {
    // No PHP / no overrides: do not silently drop candidates; install-time
    // checks will error with a clear remediation message.
    if ignore_all || !platform.reliable {
        return true;
    }
    check_requirements_filtered(platform, &locked.require, ignore_patterns).is_ok()
}

/// Reject solutions where an installed package violates another's `conflict` map.
/// Kept as a safety net after encoding conflicts in the PubGrub graph.
fn validate_conflicts(selected: &HashMap<String, LockedPackage>) -> Result<()> {
    for (name, pkg) in selected {
        for (other_name, constraint_str) in &pkg.conflict {
            let Some(other) = selected.get(other_name.as_str()) else {
                continue;
            };
            let constraint = VersionConstraint::new(constraint_str);
            let other_version = ComposerVersion::parse(&other.version)
                .map_err(|e| Error::Resolve(e.to_string()))?;
            if constraint.matches(&other_version) {
                return Err(Error::Resolve(format!(
                    "package {name}@{} conflicts with {other_name}@{} (constraint {constraint_str})",
                    pkg.version, other.version
                )));
            }
        }
    }
    Ok(())
}

fn validate_root_conflicts(
    selected: &HashMap<String, LockedPackage>,
    root_conflicts: &[(PackageId, VersionConstraint)],
) -> Result<()> {
    for (id, constraint) in root_conflicts {
        let Some(other) = selected.get(id.as_str()) else {
            continue;
        };
        let other_version =
            ComposerVersion::parse(&other.version).map_err(|e| Error::Resolve(e.to_string()))?;
        if constraint.matches(&other_version) {
            return Err(Error::Resolve(format!(
                "root project conflicts with {}@{} (constraint {})",
                id, other.version, constraint
            )));
        }
    }
    Ok(())
}

fn conflict_dummy_name(left: &str, right: &str, constraint: &str) -> String {
    format!("{CONFLICT_PREFIX}{left}::{right}::{constraint}")
}

fn ensure_conflict_dummy(
    graph: &mut HashMap<
        String,
        Vec<(
            ComposerVersion,
            DependencyConstraints<String, Ranges<ComposerVersion>>,
        )>,
    >,
    name: &str,
) {
    if graph.contains_key(name) {
        return;
    }
    let v1 = ComposerVersion::parse("1.0.0").expect("1.0.0 parses");
    let v2 = ComposerVersion::parse("2.0.0").expect("2.0.0 parses");
    graph.insert(
        name.to_string(),
        vec![
            (v1, DependencyConstraints::default()),
            (v2, DependencyConstraints::default()),
        ],
    );
}

fn add_dep_on_version(
    graph: &mut HashMap<
        String,
        Vec<(
            ComposerVersion,
            DependencyConstraints<String, Ranges<ComposerVersion>>,
        )>,
    >,
    package: &str,
    version: &ComposerVersion,
    dep: &str,
    range: Ranges<ComposerVersion>,
) {
    if let Some(versions) = graph.get_mut(package) {
        for (v, deps) in versions {
            if v == version {
                deps.insert(dep.to_string(), range);
                return;
            }
        }
    }
}

fn add_dep_on_matching(
    graph: &mut HashMap<
        String,
        Vec<(
            ComposerVersion,
            DependencyConstraints<String, Ranges<ComposerVersion>>,
        )>,
    >,
    package: &str,
    constraint: &VersionConstraint,
    dep: &str,
    range: &Ranges<ComposerVersion>,
) {
    if let Some(versions) = graph.get_mut(package) {
        for (v, deps) in versions {
            if constraint.matches(v) {
                deps.insert(dep.to_string(), range.clone());
            }
        }
    }
}

/// Custom provider so we can prefer-stable / prefer-lowest and skip platform pkgs.
struct ComposerOfflineProvider {
    /// package → (version → deps)
    graph: HashMap<
        String,
        Vec<(
            ComposerVersion,
            DependencyConstraints<String, Ranges<ComposerVersion>>,
        )>,
    >,
    prefer_lowest: bool,
    prefer_stable: bool,
}

impl ComposerOfflineProvider {
    fn build(index: &PackageIndex, request: &SolveRequest) -> Result<Self> {
        let mut graph: HashMap<
            String,
            Vec<(
                ComposerVersion,
                DependencyConstraints<String, Ranges<ComposerVersion>>,
            )>,
        > = HashMap::new();

        // Root package
        let mut root_deps: DependencyConstraints<String, Ranges<ComposerVersion>> =
            DependencyConstraints::default();
        let satisfied: std::collections::HashSet<String> = request
            .root_replace
            .iter()
            .chain(request.root_provide.iter())
            .cloned()
            .collect();

        for (id, constraint) in &request.root_deps {
            if id.is_platform() || satisfied.contains(id.as_str()) {
                continue;
            }
            root_deps.insert(id.to_string(), constraint_to_ranges(constraint));
        }
        let root_ver = ComposerVersion::parse("1.0.0").unwrap();
        graph.insert(ROOT.into(), vec![(root_ver, root_deps)]);

        let mut pending_conflicts: Vec<(String, ComposerVersion, String, VersionConstraint)> =
            Vec::new();

        for name in index.package_names() {
            let mut versions = Vec::new();
            for iv in index.all_versions(name) {
                if !version_matches_platform(
                    &iv.locked,
                    &request.platform,
                    request.ignore_platform_reqs,
                    &request.ignore_platform_req,
                ) {
                    continue;
                }

                if iv.version.stability() < request.minimum_stability
                    && !iv.version.raw.starts_with("dev-")
                {
                    // keep dev path packages even if min is stable when raw is dev-*
                    if iv.source == crate::index::IndexSource::Packagist {
                        continue;
                    }
                }

                let mut deps: DependencyConstraints<String, Ranges<ComposerVersion>> =
                    DependencyConstraints::default();
                for (dep_name, dep_c) in &iv.locked.require {
                    let dep_id = match PackageId::parse(dep_name) {
                        Ok(id) => id,
                        Err(_) => continue,
                    };
                    if dep_id.is_platform() {
                        continue;
                    }
                    deps.insert(
                        dep_name.clone(),
                        constraint_to_ranges(&VersionConstraint::new(dep_c)),
                    );
                }
                for (other, constraint_str) in &iv.locked.conflict {
                    if PackageId::parse(other).is_ok_and(|p| p.is_platform()) {
                        continue;
                    }
                    pending_conflicts.push((
                        name.clone(),
                        iv.version.clone(),
                        other.clone(),
                        VersionConstraint::new(constraint_str),
                    ));
                }

                versions.push((iv.version.clone(), deps));
            }

            if !versions.is_empty() {
                // Sort for deterministic choose_version scanning
                versions.sort_by(|a, b| a.0.cmp(&b.0));
                graph.insert(name.clone(), versions);
            }
        }

        // Encode A conflicts B as a mutex dummy rather than a positive dep on B.
        // That keeps B out of the graph unless some real require edge selects it.
        let dummy_one = Ranges::singleton(ComposerVersion::parse("1.0.0").expect("1.0.0 parses"));
        let dummy_two = Ranges::singleton(ComposerVersion::parse("2.0.0").expect("2.0.0 parses"));

        for (from, from_ver, other, constraint) in &pending_conflicts {
            let dummy = conflict_dummy_name(from, other, constraint.as_str());
            ensure_conflict_dummy(&mut graph, &dummy);
            add_dep_on_version(&mut graph, from, from_ver, &dummy, dummy_one.clone());
            add_dep_on_matching(&mut graph, other, constraint, &dummy, &dummy_two);
        }

        for (id, constraint) in &request.root_conflicts {
            if id.is_platform() {
                continue;
            }
            let dummy = conflict_dummy_name(ROOT, id.as_str(), constraint.as_str());
            ensure_conflict_dummy(&mut graph, &dummy);
            add_dep_on_version(
                &mut graph,
                ROOT,
                &ComposerVersion::parse("1.0.0").expect("1.0.0 parses"),
                &dummy,
                dummy_one.clone(),
            );
            add_dep_on_matching(&mut graph, id.as_str(), constraint, &dummy, &dummy_two);
        }

        let _ = request.minimum_stability;
        Ok(Self {
            graph,
            prefer_lowest: request.prefer_lowest,
            prefer_stable: request.prefer_stable,
        })
    }
}

impl DependencyProvider for ComposerOfflineProvider {
    type P = String;
    type V = ComposerVersion;
    type VS = Ranges<ComposerVersion>;
    type M = String;
    type Err = Infallible;
    type Priority = std::cmp::Reverse<usize>;

    fn prioritize(
        &self,
        package: &Self::P,
        range: &Self::VS,
        _stats: &PackageResolutionStatistics,
    ) -> Self::Priority {
        let count = self
            .graph
            .get(package)
            .map(|vers| vers.iter().filter(|(v, _)| range.contains(v)).count())
            .unwrap_or(0);
        std::cmp::Reverse(count)
    }

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> std::result::Result<Option<Self::V>, Infallible> {
        let Some(versions) = self.graph.get(package) else {
            debug!(package = %package, "no versions in provider");
            return Ok(None);
        };

        let mut candidates: Vec<&ComposerVersion> = versions
            .iter()
            .map(|(v, _)| v)
            .filter(|v| range.contains(v))
            .collect();

        if candidates.is_empty() {
            return Ok(None);
        }

        if self.prefer_stable {
            let stable: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|v| v.is_stable())
                .collect();
            if !stable.is_empty() {
                candidates = stable;
            }
        }

        if self.prefer_lowest {
            candidates.sort();
            Ok(candidates.first().copied().cloned())
        } else {
            candidates.sort();
            Ok(candidates.last().copied().cloned())
        }
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> std::result::Result<Dependencies<Self::P, Self::VS, Self::M>, Infallible> {
        let Some(versions) = self.graph.get(package) else {
            return Ok(Dependencies::Available(DependencyConstraints::default()));
        };
        for (v, deps) in versions {
            if v == version {
                return Ok(Dependencies::Available(deps.clone()));
            }
        }
        Ok(Dependencies::Available(DependencyConstraints::default()))
    }
}

// Silence unused import if OfflineDependencyProvider not used
#[allow(dead_code)]
fn _keep() {
    let _ = std::any::type_name::<OfflineDependencyProvider<String, Ranges<u32>>>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::PackageIndex;
    use composer_lock::{DistInfo, LockedPackage};
    use std::collections::BTreeMap;

    fn pkg(name: &str, ver: &str, require: &[(&str, &str)]) -> LockedPackage {
        let mut req = BTreeMap::new();
        for (k, v) in require {
            req.insert((*k).into(), (*v).into());
        }
        LockedPackage {
            name: name.into(),
            version: ver.into(),
            source: None,
            dist: Some(DistInfo {
                dist_type: "zip".into(),
                url: format!("https://example.com/{name}/{ver}.zip"),
                reference: None,
                shasum: None,
                mirrors: None,
            }),
            require: req,
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
    fn solves_simple_diamond() {
        let mut index = PackageIndex::new();
        // a@1 depends on c ^1
        // b@1 depends on c ^1
        // c has 1.0 and 2.0
        index.insert_locked(pkg("vendor/a", "1.0.0", &[("vendor/c", "^1.0")]));
        index.insert_locked(pkg("vendor/b", "1.0.0", &[("vendor/c", "^1.0")]));
        index.insert_locked(pkg("vendor/c", "1.0.0", &[]));
        index.insert_locked(pkg("vendor/c", "2.0.0", &[]));

        let request = SolveRequest {
            root_deps: vec![
                (
                    PackageId::parse("vendor/a").unwrap(),
                    VersionConstraint::new("^1.0"),
                ),
                (
                    PackageId::parse("vendor/b").unwrap(),
                    VersionConstraint::new("^1.0"),
                ),
            ],
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: Stability::Stable,
            root_replace: vec![],
            root_provide: vec![],
            root_conflicts: vec![],
            platform: Platform::with_php("8.2.0").unwrap(),
            ignore_platform_reqs: false,
            ignore_platform_req: vec![],
        };

        let sol = solve_with_pubgrub(&index, &request).unwrap();
        assert_eq!(sol["vendor/c"].version, "1.0.0"); // ^1 from both, not 2.0
        assert!(sol.contains_key("vendor/a"));
        assert!(sol.contains_key("vendor/b"));
    }

    #[test]
    fn conflict_detected() {
        let mut index = PackageIndex::new();
        index.insert_locked(pkg("vendor/a", "1.0.0", &[("vendor/c", "^1.0")]));
        index.insert_locked(pkg("vendor/b", "1.0.0", &[("vendor/c", "^2.0")]));
        index.insert_locked(pkg("vendor/c", "1.0.0", &[]));
        index.insert_locked(pkg("vendor/c", "2.0.0", &[]));

        let request = SolveRequest {
            root_deps: vec![
                (
                    PackageId::parse("vendor/a").unwrap(),
                    VersionConstraint::new("*"),
                ),
                (
                    PackageId::parse("vendor/b").unwrap(),
                    VersionConstraint::new("*"),
                ),
            ],
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: Stability::Stable,
            root_replace: vec![],
            root_provide: vec![],
            root_conflicts: vec![],
            platform: Platform::with_php("8.2.0").unwrap(),
            ignore_platform_reqs: false,
            ignore_platform_req: vec![],
        };

        let err = solve_with_pubgrub(&index, &request).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("PubGrub") || msg.contains("could not"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn package_conflict_rejected() {
        let mut index = PackageIndex::new();
        let mut a = pkg("vendor/a", "1.0.0", &[]);
        a.conflict.insert("vendor/b".into(), "^1.0".into());
        index.insert_locked(a);
        index.insert_locked(pkg("vendor/b", "1.0.0", &[]));

        let request = SolveRequest {
            root_deps: vec![
                (
                    PackageId::parse("vendor/a").unwrap(),
                    VersionConstraint::new("*"),
                ),
                (
                    PackageId::parse("vendor/b").unwrap(),
                    VersionConstraint::new("*"),
                ),
            ],
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: Stability::Stable,
            root_replace: vec![],
            root_provide: vec![],
            root_conflicts: vec![],
            platform: Platform::with_php("8.2.0").unwrap(),
            ignore_platform_reqs: false,
            ignore_platform_req: vec![],
        };

        let err = solve_with_pubgrub(&index, &request).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflicts with") || msg.contains("PubGrub") || msg.contains("could not"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn virtual_provide_satisfies_implementation_requirement() {
        let mut index = PackageIndex::new();
        let mut guzzle = pkg("guzzlehttp/guzzle", "7.9.0", &[]);
        guzzle
            .provide
            .insert("psr/http-client-implementation".into(), "1.0".into());
        index.insert_locked(guzzle);
        index.register_virtual_packages();

        let request = SolveRequest {
            root_deps: vec![
                (
                    PackageId::parse("guzzlehttp/guzzle").unwrap(),
                    VersionConstraint::new("^7.0"),
                ),
                (
                    PackageId::parse("psr/http-client-implementation").unwrap(),
                    VersionConstraint::new("*"),
                ),
            ],
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: Stability::Stable,
            root_replace: vec![],
            root_provide: vec![],
            root_conflicts: vec![],
            platform: Platform::with_php("8.2.0").unwrap(),
            ignore_platform_reqs: false,
            ignore_platform_req: vec![],
        };

        let raw = solve_with_pubgrub(&index, &request).unwrap();
        let sol = normalize_solution(raw, &index).unwrap();
        assert!(sol.contains_key("guzzlehttp/guzzle"));
        assert!(
            !sol.contains_key("psr/http-client-implementation"),
            "virtual name must collapse onto the provider: {:?}",
            sol.keys().collect::<Vec<_>>()
        );
        assert_eq!(sol["guzzlehttp/guzzle"].version, "7.9.0");
    }

    #[test]
    fn solver_picks_non_conflicting_version() {
        let mut index = PackageIndex::new();
        let mut a = pkg("vendor/a", "1.0.0", &[]);
        a.conflict.insert("vendor/b".into(), "^1.0".into());
        index.insert_locked(a);
        index.insert_locked(pkg("vendor/b", "1.0.0", &[]));
        index.insert_locked(pkg("vendor/b", "2.0.0", &[]));

        let request = SolveRequest {
            root_deps: vec![
                (
                    PackageId::parse("vendor/a").unwrap(),
                    VersionConstraint::new("*"),
                ),
                (
                    PackageId::parse("vendor/b").unwrap(),
                    VersionConstraint::new("*"),
                ),
            ],
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: Stability::Stable,
            root_replace: vec![],
            root_provide: vec![],
            root_conflicts: vec![],
            platform: Platform::with_php("8.2.0").unwrap(),
            ignore_platform_reqs: false,
            ignore_platform_req: vec![],
        };

        let sol = solve_with_pubgrub(&index, &request).unwrap();
        assert_eq!(sol["vendor/a"].version, "1.0.0");
        assert_eq!(
            sol["vendor/b"].version, "2.0.0",
            "solver should skip B 1.x due to A's conflict"
        );
    }

    #[test]
    fn root_conflict_rejects_matching_version() {
        let mut index = PackageIndex::new();
        index.insert_locked(pkg("acme/bad", "1.0.0", &[]));

        let request = SolveRequest {
            root_deps: vec![(
                PackageId::parse("acme/bad").unwrap(),
                VersionConstraint::new("*"),
            )],
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: Stability::Stable,
            root_replace: vec![],
            root_provide: vec![],
            root_conflicts: vec![(
                PackageId::parse("acme/bad").unwrap(),
                VersionConstraint::new("<2.0"),
            )],
            platform: Platform::with_php("8.2.0").unwrap(),
            ignore_platform_reqs: false,
            ignore_platform_req: vec![],
        };

        let err = solve_with_pubgrub(&index, &request).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflicts") || msg.contains("PubGrub") || msg.contains("could not"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn conflict_does_not_force_optional_package() {
        // A 1.0 requires B; A 2.0 has no deps. C conflicts with B ^1.
        // Valid solution: A 2.0 + C 1.0, without B.
        let mut index = PackageIndex::new();
        index.insert_locked(pkg("acme/a", "1.0.0", &[("acme/b", "^1")]));
        index.insert_locked(pkg("acme/a", "2.0.0", &[]));
        index.insert_locked(pkg("acme/b", "1.0.0", &[]));
        let mut c = pkg("acme/c", "1.0.0", &[]);
        c.conflict.insert("acme/b".into(), "^1".into());
        index.insert_locked(c);

        let request = SolveRequest {
            root_deps: vec![
                (
                    PackageId::parse("acme/a").unwrap(),
                    VersionConstraint::new("*"),
                ),
                (
                    PackageId::parse("acme/c").unwrap(),
                    VersionConstraint::new("*"),
                ),
            ],
            prefer_stable: true,
            prefer_lowest: false,
            minimum_stability: Stability::Stable,
            root_replace: vec![],
            root_provide: vec![],
            root_conflicts: vec![],
            platform: Platform::with_php("8.2.0").unwrap(),
            ignore_platform_reqs: false,
            ignore_platform_req: vec![],
        };

        let sol = solve_with_pubgrub(&index, &request).unwrap();
        assert_eq!(sol["acme/a"].version, "2.0.0");
        assert_eq!(sol["acme/c"].version, "1.0.0");
        assert!(
            !sol.contains_key("acme/b"),
            "B must stay optional: {:?}",
            sol.keys().collect::<Vec<_>>()
        );
    }
}
