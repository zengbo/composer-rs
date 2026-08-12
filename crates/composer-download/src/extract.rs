//! Archive extraction (zip / tar / tar.gz / tar.bz2 / tar.xz).

use crate::archive::ArchiveType;
use composer_core::error::{Error, Result};
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tracing::debug;

/// Extract archive into `dest`, creating directories as needed.
pub fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|e| Error::io(dest, e))?;

    let kind = ArchiveType::from_path(archive).unwrap_or_else(|| detect_by_magic(archive));

    debug!(archive = %archive.display(), ?kind, dest = %dest.display(), "extracting");

    match kind {
        ArchiveType::Zip => extract_zip(archive, dest),
        ArchiveType::Tar => extract_tar(
            File::open(archive).map_err(|e| Error::io(archive, e))?,
            dest,
        ),
        ArchiveType::TarGz => {
            let file = File::open(archive).map_err(|e| Error::io(archive, e))?;
            extract_tar(GzDecoder::new(file), dest)
        }
        ArchiveType::TarBz2 => {
            let file = File::open(archive).map_err(|e| Error::io(archive, e))?;
            extract_tar(bzip2::read::BzDecoder::new(file), dest)
        }
        ArchiveType::TarXz => {
            let file = File::open(archive).map_err(|e| Error::io(archive, e))?;
            extract_tar(xz2::read::XzDecoder::new(file), dest)
        }
    }
}

fn detect_by_magic(path: &Path) -> ArchiveType {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return ArchiveType::Zip,
    };
    let mut magic = [0u8; 6];
    let _ = file.read(&mut magic);
    let _ = file.seek(SeekFrom::Start(0));

    if magic.starts_with(b"PK") {
        ArchiveType::Zip
    } else if magic.starts_with(&[0x1f, 0x8b]) {
        ArchiveType::TarGz
    } else if magic.starts_with(b"BZh") {
        ArchiveType::TarBz2
    } else if magic.starts_with(&[0xfd, b'7', b'z', b'X', b'Z']) {
        ArchiveType::TarXz
    } else {
        ArchiveType::Zip
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive).map_err(|e| Error::io(archive, e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| Error::archive(e.to_string()))?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| Error::archive(e.to_string()))?;
        let name = entry.name().to_string();

        // Zip-slip protection
        let out_path = safe_join(dest, &name)?;
        if entry.is_dir() || name.ends_with('/') {
            std::fs::create_dir_all(&out_path).map_err(|e| Error::io(&out_path, e))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let mut outfile = File::create(&out_path).map_err(|e| Error::io(&out_path, e))?;
        std::io::copy(&mut entry, &mut outfile).map_err(|e| Error::io(&out_path, e))?;

        // Best-effort unix mode
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn extract_tar(reader: impl Read, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .map_err(|e| Error::archive(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| Error::archive(e.to_string()))?;
        let raw = entry
            .path()
            .map_err(|e| Error::archive(e.to_string()))?
            .to_string_lossy()
            .into_owned();
        let out_path = safe_join(dest, &raw)?;

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| Error::io(&out_path, e))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let mut outfile = File::create(&out_path).map_err(|e| Error::io(&out_path, e))?;
        std::io::copy(&mut entry, &mut outfile).map_err(|e| Error::io(&out_path, e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mode) = entry.header().mode() {
                let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn safe_join(base: &Path, name: &str) -> Result<std::path::PathBuf> {
    let mut out = base.to_path_buf();
    for comp in Path::new(name).components() {
        match comp {
            std::path::Component::Normal(c) => out.push(c),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(Error::archive(format!(
                    "refusing path traversal in archive entry: {name}"
                )));
            }
            _ => {}
        }
    }
    // Ensure still under base
    if !out.starts_with(base) {
        return Err(Error::archive(format!(
            "refusing path outside dest: {name}"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_blocks_parent_segments() {
        let base = tempfile::tempdir().unwrap();
        let err = safe_join(base.path(), "../outside.txt").unwrap_err();
        assert!(
            err.to_string().contains("traversal"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn safe_join_allows_normal_paths() {
        let base = tempfile::tempdir().unwrap();
        let out = safe_join(base.path(), "src/Foo.php").unwrap();
        assert!(out.starts_with(base.path()));
        assert!(out.ends_with("Foo.php"));
    }
}
