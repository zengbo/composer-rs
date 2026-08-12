//! Shared error types.

use std::path::PathBuf;
use thiserror::Error;

/// Result alias for composer-rs core operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Domain errors used across crates.
#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid package name: {0}")]
    InvalidPackageName(String),

    #[error("invalid version constraint: {0}")]
    InvalidConstraint(String),

    #[error("invalid version: {0}")]
    InvalidVersion(String),

    #[error("package not found: {0}")]
    PackageNotFound(String),

    #[error("no matching version for {package} satisfying {constraint}")]
    NoMatchingVersion { package: String, constraint: String },

    #[error("dependency resolution failed: {0}")]
    Resolve(String),

    #[error("download failed for {url}: {message}")]
    Download { url: String, message: String },

    #[error("checksum mismatch for {package}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        package: String,
        expected: String,
        actual: String,
    },

    #[error("archive error: {0}")]
    Archive(String),

    #[error("cache error: {0}")]
    Cache(String),

    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("lockfile error: {0}")]
    Lockfile(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn archive(msg: impl Into<String>) -> Self {
        Self::Archive(msg.into())
    }

    pub fn download(url: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Download {
            url: url.into(),
            message: message.into(),
        }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
