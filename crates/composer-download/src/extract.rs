//! Archive extraction (zip / tar / tar.gz / tar.bz2 / tar.xz).

use crate::archive::ArchiveType;
use composer_core::error::{Error, Result};
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tracing::debug;

const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_UNCOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_PATH_DEPTH: usize = 32;

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
    if zip.len() > MAX_ARCHIVE_ENTRIES {
        return Err(Error::archive(format!(
            "archive has {} entries (limit {MAX_ARCHIVE_ENTRIES})",
            zip.len()
        )));
    }

    let mut expanded = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| Error::archive(e.to_string()))?;
        let name = entry.name().to_string();
        check_path_depth(&name)?;

        let out_path = safe_join(dest, &name)?;
        if entry.is_dir() || name.ends_with('/') {
            std::fs::create_dir_all(&out_path).map_err(|e| Error::io(&out_path, e))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        #[cfg(unix)]
        if is_unix_symlink(entry.unix_mode()) {
            let mut target = String::new();
            entry
                .read_to_string(&mut target)
                .map_err(|e| Error::archive(e.to_string()))?;
            create_relative_symlink(&out_path, dest, target.trim())?;
            continue;
        }

        let size = entry.size();
        expanded = expanded.saturating_add(size);
        if expanded > MAX_UNCOMPRESSED_BYTES {
            return Err(Error::archive(format!(
                "archive expands beyond {MAX_UNCOMPRESSED_BYTES} bytes"
            )));
        }

        let mut outfile = File::create(&out_path).map_err(|e| Error::io(&out_path, e))?;
        std::io::copy(&mut entry, &mut outfile).map_err(|e| Error::io(&out_path, e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = std::fs::set_permissions(
                    &out_path,
                    std::fs::Permissions::from_mode(mode & 0o777),
                );
            }
        }
    }
    Ok(())
}

fn extract_tar(reader: impl Read, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut entries = 0usize;
    let mut expanded = 0u64;
    for entry in archive
        .entries()
        .map_err(|e| Error::archive(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| Error::archive(e.to_string()))?;
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            return Err(Error::archive(format!(
                "archive has more than {MAX_ARCHIVE_ENTRIES} entries"
            )));
        }
        let raw = entry
            .path()
            .map_err(|e| Error::archive(e.to_string()))?
            .to_string_lossy()
            .into_owned();
        check_path_depth(&raw)?;
        let out_path = safe_join(dest, &raw)?;
        let kind = entry.header().entry_type();

        if kind.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| Error::io(&out_path, e))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        if kind.is_symlink() || kind.is_hard_link() {
            let target = entry
                .link_name()
                .map_err(|e| Error::archive(e.to_string()))?
                .ok_or_else(|| Error::archive(format!("link entry missing target: {raw}")))?;
            create_relative_symlink(&out_path, dest, &target.to_string_lossy())?;
            continue;
        }

        let size = entry.header().size().unwrap_or(0);
        expanded = expanded.saturating_add(size);
        if expanded > MAX_UNCOMPRESSED_BYTES {
            return Err(Error::archive(format!(
                "archive expands beyond {MAX_UNCOMPRESSED_BYTES} bytes"
            )));
        }

        let mut outfile = File::create(&out_path).map_err(|e| Error::io(&out_path, e))?;
        std::io::copy(&mut entry, &mut outfile).map_err(|e| Error::io(&out_path, e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mode) = entry.header().mode() {
                let _ = std::fs::set_permissions(
                    &out_path,
                    std::fs::Permissions::from_mode(mode & 0o777),
                );
            }
        }
    }
    Ok(())
}

fn check_path_depth(name: &str) -> Result<()> {
    let depth = Path::new(name)
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    if depth > MAX_PATH_DEPTH {
        return Err(Error::archive(format!(
            "refusing archive path deeper than {MAX_PATH_DEPTH}: {name}"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn is_unix_symlink(mode: Option<u32>) -> bool {
    mode.is_some_and(|m| m & 0o170000 == 0o120000)
}

fn create_relative_symlink(out_path: &Path, dest_root: &Path, target: &str) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        return Err(Error::archive("empty symlink target"));
    }
    let target_path = Path::new(target);
    if target_path.is_absolute() {
        return Err(Error::archive(format!(
            "refusing absolute symlink target: {target}"
        )));
    }
    let parent = out_path.parent().unwrap_or(dest_root);
    let resolved = safe_join(parent, target)?;
    if !resolved.starts_with(dest_root) {
        return Err(Error::archive(format!(
            "refusing symlink that escapes extract root: {target}"
        )));
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    #[cfg(unix)]
    {
        if out_path.exists() || out_path.is_symlink() {
            let _ = std::fs::remove_file(out_path);
        }
        std::os::unix::fs::symlink(target, out_path).map_err(|e| Error::io(out_path, e))?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let _ = dest_root;
        if resolved.is_file() {
            std::fs::copy(&resolved, out_path).map_err(|e| Error::io(out_path, e))?;
            return Ok(());
        }
        Err(Error::archive(format!(
            "cannot recreate symlink on this platform: {target}"
        )))
    }
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

    #[test]
    fn extract_tar_preserves_relative_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("pkg.tar");
        {
            let file = File::create(&archive).unwrap();
            let mut builder = tar::Builder::new(file);
            let mut header = tar::Header::new_gnu();
            header.set_size(5);
            header.set_cksum();
            builder
                .append_data(&mut header, "hello.txt", b"hello".as_slice())
                .unwrap();
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            link.set_cksum();
            builder
                .append_link(&mut link, "alias.txt", "hello.txt")
                .unwrap();
            builder.finish().unwrap();
        }
        let dest = tmp.path().join("out");
        extract_archive(&archive, &dest).unwrap();
        let alias = dest.join("alias.txt");
        assert!(alias.is_symlink() || alias.is_file());
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(&alias).unwrap(), Path::new("hello.txt"));
        assert_eq!(
            std::fs::read_to_string(dest.join("hello.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn extract_rejects_absolute_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("bad.tar");
        {
            let file = File::create(&archive).unwrap();
            let mut builder = tar::Builder::new(file);
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            link.set_cksum();
            builder
                .append_link(&mut link, "evil", "/etc/passwd")
                .unwrap();
            builder.finish().unwrap();
        }
        let dest = tmp.path().join("out");
        let err = extract_archive(&archive, &dest).unwrap_err();
        assert!(
            err.to_string().contains("absolute") || err.to_string().contains("symlink"),
            "{err}"
        );
    }
}
