//! `composer-rs depends` / `why` / `prohibits` / `why-not`

use super::{info, project_paths};
use anyhow::{Result, bail};
use clap::Args;
use composer_core::{ComposerVersion, PackageId, VersionConstraint};
use composer_lock::{ComposerLock, LockedPackage};
use composer_manifest::ComposerJson;
use composer_repo::RepositoryRegistry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Args, Debug, Clone)]
pub struct DependsArgs {
    /// Package name
    pub package: String,

    /// Optional version constraint for `prohibits` / `why-not`
    pub version: Option<String>,

    /// Recursive (show tree)
    #[arg(long, short = 'r')]
    pub recursive: bool,

    /// Tree output
    #[arg(long, short = 't')]
    pub tree: bool,

    #[arg(long)]
    pub no_dev: bool,
}

/// Edge label for reverse / forward dependency graphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    Require,
    RequireDev,
}

impl DepKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Require => "requires",
            Self::RequireDev => "require-dev",
        }
    }
}

/// Reverse edges: package → list of (dependent, constraint, kind).
pub fn reverse_dependents(
    packages: &[&LockedPackage],
    with_dev: bool,
) -> BTreeMap<String, Vec<(String, String, DepKind)>> {
    let mut reverse: BTreeMap<String, Vec<(String, String, DepKind)>> = BTreeMap::new();
    for pkg in packages {
        for (dep, c) in &pkg.require {
            if PackageId::parse(dep).is_ok_and(|p| p.is_platform()) {
                continue;
            }
            reverse.entry(dep.clone()).or_default().push((
                pkg.name.clone(),
                c.clone(),
                DepKind::Require,
            ));
        }
        if with_dev {
            for (dep, c) in &pkg.require_dev {
                if PackageId::parse(dep).is_ok_and(|p| p.is_platform()) {
                    continue;
                }
                reverse.entry(dep.clone()).or_default().push((
                    pkg.name.clone(),
                    c.clone(),
                    DepKind::RequireDev,
                ));
            }
        }
    }
    reverse
}

/// Forward edges: package → list of non-platform dependency names.
/// When `with_dev`, also includes `require-dev` keys.
pub fn forward_deps(packages: &[&LockedPackage], with_dev: bool) -> BTreeMap<String, Vec<String>> {
    let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pkg in packages {
        let mut deps: Vec<String> = pkg
            .require
            .keys()
            .filter(|k| PackageId::parse(k).is_ok_and(|p| !p.is_platform()))
            .cloned()
            .collect();
        if with_dev {
            for k in pkg.require_dev.keys() {
                if PackageId::parse(k).is_ok_and(|p| !p.is_platform()) {
                    deps.push(k.clone());
                }
            }
        }
        deps.sort();
        deps.dedup();
        graph.insert(pkg.name.clone(), deps);
    }
    graph
}

/// BFS path from a root requirement to `target` (inclusive), if any.
pub fn why_chain(
    roots: &[String],
    graph: &BTreeMap<String, Vec<String>>,
    target: &str,
) -> Option<Vec<String>> {
    let mut parent: BTreeMap<String, String> = BTreeMap::new();
    let mut q = VecDeque::new();
    for r in roots {
        q.push_back(r.clone());
        parent.entry(r.clone()).or_insert_with(String::new);
    }
    while let Some(cur) = q.pop_front() {
        if let Some(deps) = graph.get(&cur) {
            for d in deps {
                if !parent.contains_key(d) {
                    parent.insert(d.clone(), cur.clone());
                    q.push_back(d.clone());
                }
            }
        }
    }

    if !parent.contains_key(target) && !roots.iter().any(|r| r == target) {
        return None;
    }

    let mut chain = vec![target.to_string()];
    let mut cur = target.to_string();
    while let Some(p) = parent.get(&cur) {
        if p.is_empty() {
            break;
        }
        chain.push(p.clone());
        cur = p.clone();
    }
    chain.reverse();
    Some(chain)
}

pub async fn run_depends(args: DependsArgs) -> Result<()> {
    let (_cwd, _json, lock_path) = project_paths()?;
    if !lock_path.exists() {
        bail!("composer.lock not found");
    }
    let lock = ComposerLock::load(&lock_path)?;
    let with_dev = !args.no_dev;
    let packages = lock.packages_to_install(with_dev);
    let reverse = reverse_dependents(&packages, with_dev);

    let target = &args.package;
    if args.tree || args.recursive {
        print_tree(&reverse, target, "", &mut BTreeSet::new(), 0);
    } else if let Some(dependents) = reverse.get(target) {
        for (name, c, kind) in dependents {
            println!("{name} {} {target} ({c})", kind.as_str());
        }
    } else {
        info(&format!("No locked packages require {target}"));
    }
    Ok(())
}

fn print_tree(
    reverse: &BTreeMap<String, Vec<(String, String, DepKind)>>,
    name: &str,
    prefix: &str,
    seen: &mut BTreeSet<String>,
    depth: usize,
) {
    if depth > 20 || !seen.insert(name.to_string()) {
        return;
    }
    if let Some(deps) = reverse.get(name) {
        for (i, (parent, c, kind)) in deps.iter().enumerate() {
            let last = i + 1 == deps.len();
            let branch = if last { "└──" } else { "├──" };
            println!("{prefix}{branch} {parent} ({} {name} {c})", kind.as_str());
            let next = format!("{prefix}{}", if last { "    " } else { "│   " });
            print_tree(reverse, parent, &next, seen, depth + 1);
        }
    }
}

/// `why` — dependency chain from root to package.
pub async fn run_why(args: DependsArgs) -> Result<()> {
    let (_cwd, json_path, lock_path) = project_paths()?;
    if !lock_path.exists() {
        bail!("composer.lock not found");
    }
    let manifest = ComposerJson::load(&json_path)?;
    let lock = ComposerLock::load(&lock_path)?;
    let with_dev = !args.no_dev;
    let packages = lock.packages_to_install(with_dev);
    let graph = forward_deps(&packages, with_dev);

    let mut roots: Vec<String> = manifest
        .require
        .keys()
        .filter(|k| PackageId::parse(k).is_ok_and(|p| !p.is_platform()))
        .cloned()
        .collect();
    if with_dev {
        roots.extend(
            manifest
                .require_dev
                .keys()
                .filter(|k| PackageId::parse(k).is_ok_and(|p| !p.is_platform()))
                .cloned(),
        );
    }

    let Some(chain) = why_chain(&roots, &graph, &args.package) else {
        bail!("{} is not in the installed dependency graph", args.package);
    };
    println!("{}", chain.join(" → "));
    Ok(())
}

/// `prohibits` / `why-not` — explain why a version cannot be installed.
pub async fn run_prohibits(args: DependsArgs) -> Result<()> {
    let (cwd, json_path, lock_path) = project_paths()?;
    if !json_path.exists() {
        bail!("composer.json not found");
    }
    let ver = args
        .version
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("version constraint required (e.g. why-not foo/bar 2.0)"))?;
    let manifest = ComposerJson::load(&json_path)?;
    let lock = if lock_path.exists() {
        Some(ComposerLock::load(&lock_path)?)
    } else {
        None
    };

    let want = ComposerVersion::parse(ver)
        .or_else(|_| ComposerVersion::parse(ver.trim_start_matches('^').trim_start_matches('~')))?;

    if let Some(c) = manifest
        .require
        .get(&args.package)
        .or_else(|| manifest.require_dev.get(&args.package))
    {
        let vc = VersionConstraint::new(c.clone());
        if !vc.matches(&want) {
            println!(
                "root requires {} ({c}) which does not allow {ver}",
                args.package
            );
            return Ok(());
        }
    }

    let with_dev = !args.no_dev;
    if let Some(lock) = &lock {
        let mut blocked = false;
        for pkg in lock.packages_to_install(with_dev) {
            for (map, kind) in [
                (&pkg.require, DepKind::Require),
                (&pkg.require_dev, DepKind::RequireDev),
            ] {
                if !with_dev && kind == DepKind::RequireDev {
                    continue;
                }
                if let Some(c) = map.get(&args.package) {
                    let vc = VersionConstraint::new(c.clone());
                    if !vc.matches(&want) {
                        println!(
                            "{} {} {} {} ({c}) — conflicts with {ver}",
                            pkg.name,
                            pkg.version,
                            kind.as_str(),
                            args.package
                        );
                        blocked = true;
                    }
                }
            }
        }
        if blocked {
            return Ok(());
        }
    }

    let registry = RepositoryRegistry::from_manifest_auth(
        &manifest,
        composer_auth::AuthStore::load(Some(&cwd)).unwrap_or_default(),
    )?;
    let id = PackageId::parse(&args.package)?;
    match registry.get_package_versions(&id).await {
        Ok(versions) => {
            let found = versions
                .iter()
                .any(|v| v.version.raw == ver || v.version == want);
            if !found {
                println!(
                    "No version matching {ver} found for {} on configured repositories",
                    args.package
                );
            } else {
                info(&format!(
                    "No hard conflicts found for {} {ver} against the current lock/root constraints",
                    args.package
                ));
            }
        }
        Err(e) => println!("Could not query registry: {e}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use composer_lock::{DistInfo, LockedPackage};
    use std::collections::BTreeMap;

    fn pkg(name: &str, require: &[(&str, &str)], require_dev: &[(&str, &str)]) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: "1.0.0".into(),
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
    fn reverse_includes_require_dev_when_with_dev() {
        let a = pkg("acme/a", &[], &[("acme/phpunit", "^10")]);
        let phpunit = pkg("acme/phpunit", &[], &[]);
        let pkgs = vec![&a, &phpunit];
        let rev = reverse_dependents(&pkgs, true);
        let deps = rev.get("acme/phpunit").expect("phpunit dependents");
        assert!(
            deps.iter()
                .any(|(n, _, k)| n == "acme/a" && *k == DepKind::RequireDev)
        );
        let rev_no_dev = reverse_dependents(&pkgs, false);
        assert!(rev_no_dev.get("acme/phpunit").is_none());
    }

    #[test]
    fn why_chain_walks_require_dev_edge() {
        let phpunit = pkg("acme/phpunit", &[], &[("acme/util", "^1")]);
        let util = pkg("acme/util", &[], &[]);
        let pkgs = vec![&phpunit, &util];
        let graph = forward_deps(&pkgs, true);
        let chain = why_chain(&["acme/phpunit".into()], &graph, "acme/util").unwrap();
        assert_eq!(chain, vec!["acme/phpunit", "acme/util"]);

        // without with_dev, util is unreachable via require-dev-only edge
        let graph_prod = forward_deps(&pkgs, false);
        assert!(why_chain(&["acme/phpunit".into()], &graph_prod, "acme/util").is_none());
    }
}
