//! In-memory package version index for the resolver.

use crate::sources::LocalPathPackage;
use composer_core::ComposerVersion;
use composer_lock::LockedPackage;
use composer_repo::RemotePackageVersion;
use std::collections::{BTreeMap, HashMap};

/// All known versions of packages available to the solver.
#[derive(Debug, Default)]
pub struct PackageIndex {
    /// name → version_raw → locked package snapshot
    packages: BTreeMap<String, BTreeMap<String, IndexedVersion>>,
    /// (virtual_name, version_raw) → real provider package name.
    /// Supports multiple providers for the same virtual package.
    virtual_providers: HashMap<(String, String), String>,
    /// virtual_name → real providers (for diagnostics / fallback).
    virtual_provider_names: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct IndexedVersion {
    pub version: ComposerVersion,
    pub locked: LockedPackage,
    pub source: IndexSource,
    /// When this entry is a virtual provide/replace alias, the real package name.
    pub provided_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexSource {
    Packagist,
    Path,
    Vcs,
    Inline,
}

impl PackageIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_package(&self, name: &str) -> bool {
        self.packages.contains_key(name)
    }

    pub fn versions_of(&self, name: &str) -> Option<Vec<&LockedPackage>> {
        self.packages
            .get(name)
            .map(|m| m.values().map(|v| &v.locked).collect())
    }

    pub fn all_versions(&self, name: &str) -> Vec<&IndexedVersion> {
        self.packages
            .get(name)
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, name: &str, version_raw: &str) -> Option<&IndexedVersion> {
        self.packages.get(name)?.get(version_raw)
    }

    pub fn insert_remote(&mut self, name: &str, versions: Vec<RemotePackageVersion>) {
        let entry = self.packages.entry(name.to_string()).or_default();
        for v in versions {
            let locked = v.to_locked();
            entry.insert(
                v.version.raw.clone(),
                IndexedVersion {
                    version: v.version,
                    locked,
                    source: IndexSource::Packagist,
                    provided_by: None,
                },
            );
        }
    }

    pub fn insert_local(&mut self, pkg: LocalPathPackage) {
        let entry = self.packages.entry(pkg.name.clone()).or_default();
        entry.insert(
            pkg.version.raw.clone(),
            IndexedVersion {
                version: pkg.version.clone(),
                locked: pkg.to_locked(),
                source: pkg.source_kind.into(),
                provided_by: None,
            },
        );
    }

    pub fn insert_locked(&mut self, locked: LockedPackage) {
        self.insert_locked_with_provider(locked, None);
    }

    fn insert_locked_with_provider(&mut self, locked: LockedPackage, provided_by: Option<String>) {
        let version = ComposerVersion::parse(&locked.version).unwrap_or_else(|_| {
            ComposerVersion::parse("0.0.0").expect("0.0.0 parses")
        });
        // For virtual aliases, disambiguate colliding version keys from different providers.
        let key = if let Some(ref real) = provided_by {
            format!("{}@{}", locked.version, real)
        } else {
            locked.version.clone()
        };
        let entry = self.packages.entry(locked.name.clone()).or_default();
        entry.insert(
            key,
            IndexedVersion {
                version,
                locked,
                source: IndexSource::Inline,
                provided_by,
            },
        );
    }

    pub fn package_names(&self) -> impl Iterator<Item = &String> {
        self.packages.keys()
    }

    /// Register virtual packages from all `provide` / `replace` declarations.
    ///
    /// Multiple real packages may provide the same virtual name; each contributes
    /// its own version entry so the solver can pick any satisfying provider.
    pub fn register_virtual_packages(&mut self) {
        let providers: Vec<IndexedVersion> = self
            .packages
            .values()
            .flat_map(|versions| versions.values().cloned())
            .filter(|iv| iv.provided_by.is_none()) // only real packages
            .collect();

        for iv in providers {
            let real_name = iv.locked.name.clone();
            let mut virtuals: Vec<(String, String)> = iv
                .locked
                .provide
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            virtuals.extend(
                iv.locked
                    .replace
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );

            for (virtual_name, virtual_constraint) in virtuals {
                if virtual_name.is_empty() || virtual_name == real_name {
                    continue;
                }
                // Prefer a concrete version for the alias: use provider version when
                // provide value is a constraint like `*` or `^1.0`.
                let virtual_version = if virtual_constraint == "*"
                    || virtual_constraint.is_empty()
                    || virtual_constraint.starts_with('^')
                    || virtual_constraint.starts_with('~')
                    || virtual_constraint.starts_with('>')
                    || virtual_constraint.starts_with('<')
                    || virtual_constraint.contains('|')
                {
                    iv.locked.version.clone()
                } else {
                    virtual_constraint.trim_start_matches('=').to_string()
                };

                let mut locked = iv.locked.clone();
                locked.name = virtual_name.clone();
                locked.version = virtual_version.clone();
                // Virtual entries shouldn't re-provide themselves in a loop.
                locked.provide.clear();
                locked.replace.clear();

                self.virtual_providers
                    .insert((virtual_name.clone(), virtual_version.clone()), real_name.clone());
                self.virtual_provider_names
                    .entry(virtual_name)
                    .or_default()
                    .push(real_name.clone());

                self.insert_locked_with_provider(locked, Some(real_name.clone()));
            }
        }
    }

    /// Map a virtual package name (+ optional version) back to its provider.
    pub fn real_package_name<'a>(&'a self, name: &'a str) -> &'a str {
        // Prefer exact version mapping via provided_by on entries; fall back to first provider.
        if let Some(names) = self.virtual_provider_names.get(name) {
            if let Some(first) = names.first() {
                return first.as_str();
            }
        }
        name
    }

    /// Resolve the real provider for a selected virtual package version.
    pub fn real_provider_for(&self, virtual_name: &str, version_raw: &str) -> Option<&str> {
        if let Some(real) = self
            .virtual_providers
            .get(&(virtual_name.to_string(), version_raw.to_string()))
        {
            return Some(real.as_str());
        }
        // Key may be "version@provider"
        if let Some(iv) = self
            .all_versions(virtual_name)
            .into_iter()
            .find(|v| v.version.raw == version_raw || v.locked.version == version_raw)
        {
            if let Some(ref real) = iv.provided_by {
                return Some(real.as_str());
            }
        }
        self.virtual_provider_names
            .get(virtual_name)
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    /// Best matching locked package for a real provider name.
    pub fn provider_locked(&self, real_name: &str, hint_version: &str) -> Option<LockedPackage> {
        if let Some(iv) = self.get(real_name, hint_version) {
            return Some(iv.locked.clone());
        }
        // Hint may be virtual version; try parse-equal
        self.all_versions(real_name)
            .into_iter()
            .filter(|iv| iv.provided_by.is_none())
            .max_by(|a, b| a.version.cmp(&b.version))
            .map(|iv| iv.locked.clone())
    }
}

impl From<crate::sources::SourceKind> for IndexSource {
    fn from(k: crate::sources::SourceKind) -> Self {
        match k {
            crate::sources::SourceKind::Path => Self::Path,
            crate::sources::SourceKind::Vcs => Self::Vcs,
        }
    }
}
