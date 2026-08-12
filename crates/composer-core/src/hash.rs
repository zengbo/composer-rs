//! Content hashing (BLAKE3).

use blake3::Hasher;
use std::io::Read;
use std::path::Path;

/// 32-byte BLAKE3 content hash rendered as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn from_bytes(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        Self(hash.to_hex().to_string())
    }

    pub fn from_reader(mut reader: impl Read) -> std::io::Result<Self> {
        let mut hasher = Hasher::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(Self(hasher.finalize().to_hex().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Two-char shard prefix for directory layout (`ab/cdef...`).
    pub fn shard(&self) -> &str {
        &self.0[..2.min(self.0.len())]
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ContentHash {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Hash an arbitrary string key (e.g. dist URL + reference).
pub fn content_hash(input: impl AsRef<[u8]>) -> ContentHash {
    ContentHash::from_bytes(input.as_ref())
}

/// Hash a file on disk.
pub fn hash_file(path: &Path) -> std::io::Result<ContentHash> {
    let file = std::fs::File::open(path)?;
    ContentHash::from_reader(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let a = content_hash(b"hello");
        let b = content_hash(b"hello");
        assert_eq!(a, b);
        assert_eq!(a.shard().len(), 2);
    }
}
