//! Core types for composer-rs: package IDs, versions, errors, and hashing.

#![deny(unsafe_code)]

pub mod error;
pub mod hash;
pub mod package;
pub mod platform;
pub mod ranges;
pub mod version;

pub use error::{Error, Result};
pub use hash::{content_hash, ContentHash};
pub use package::{AutoloadConfig, PackageId, PackageType, PathOrPaths};
pub use platform::{check_requirements, Platform};
pub use ranges::constraint_to_ranges;
pub use version::{ComposerVersion, Stability, VersionConstraint};

pub use ahash::{AHashMap, AHashSet};
pub use semver::Version;
