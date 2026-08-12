//! Core types for composer-rs: package IDs, versions, errors, and hashing.

#![deny(unsafe_code)]

pub mod error;
pub mod hash;
pub mod package;
pub mod platform;
pub mod ranges;
pub mod version;

pub use error::{Error, Result};
pub use hash::{ContentHash, content_hash};
pub use package::{AutoloadConfig, PackageId, PackageType, PathOrPaths};
pub use platform::{
    Platform, check_requirements, check_requirements_filtered, platform_req_ignored,
};
pub use ranges::{conflict_to_ranges, constraint_to_ranges};
pub use version::{ComposerVersion, Stability, VersionConstraint, version_normalized};

pub use ahash::{AHashMap, AHashSet};
pub use semver::Version;
