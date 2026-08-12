//! In-memory package version index for the resolver.

use crate::sources::LocalPathPackage;
use composer_core::ComposerVersion;
use composer_lock::LockedPackage;
use composer_repo::RemotePackageVersion;
use std::collections::BTreeMap;

/// All known versions of packages available to the solver.
#[derive(Debug, Default)]
pub struct PackageIndex {
    /// name → version_raw → locked package snapshot
    packages: BTreeMap<String, BTreeMap<String, IndexedVersion>>,
}

#[derive(Debug, Clone)]
pub struct IndexedVersion {
    pub version: ComposerVersion,
    pub locked: LockedPackage,
    pub source: IndexSource,
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
            },
        );
    }

    pub fn insert_locked(&mut self, locked: LockedPackage) {
        let version = ComposerVersion::parse(&locked.version).unwrap_or_else(|_| {
            ComposerVersion::parse("0.0.0").expect("0.0.0 parses")
        });
        let entry = self.packages.entry(locked.name.clone()).or_default();
        entry.insert(
            locked.version.clone(),
            IndexedVersion {
                version,
                locked,
                source: IndexSource::Inline,
            },
        );
    }

    pub fn package_names(&self) -> impl Iterator<Item = &String> {
        self.packages.keys()
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
