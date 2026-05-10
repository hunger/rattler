//! Symlink-resistant tar extraction.
//!
//! [`unpack_safely`] is the cap-std-anchored replacement for
//! [`tar::Archive::unpack`]. Every write goes through a
//! [`cap_std::fs::Dir`] capability rooted at the destination, so
//! a tar entry that's a path-component symlink leading outside
//! the destination causes the kernel-level open to fail rather
//! than letting the next entry write through it. This closes the
//! "Zip-Slip-with-symlink" class against `.tar.bz2` and against
//! the inner `.tar.zst` of a `.conda` archive.
//!
//! Compiled on every native target rattler supports (Linux, macOS,
//! Windows; cap-std covers all three). On `wasm32` cap-std's WASI
//! support is still in development, so callers there fall through
//! to [`tar::Archive::unpack`] in [`crate::read`].
#![cfg(not(target_arch = "wasm32"))]

use std::io::Read;
use std::path::Path;

use rattler_fs_safety::{ambient_authority, Dir};
#[cfg(unix)]
use rattler_fs_safety::{Permissions, PermissionsExt};
use tar::EntryType;

use crate::ExtractError;

/// Extract every entry of `archive` into `destination`, refusing
/// any entry whose path or symlink target would escape.
///
/// The caller is responsible for creating `destination` (the
/// existing entry points already do this). Failure modes:
///
/// * [`ExtractError::UnsafeArchivePath`] -- an entry's lexical
///   path or a symlink target resolves outside `destination`.
///   Extraction stops; entries written so far remain on disk.
/// * [`ExtractError::IoError`] -- a read or write failed; same
///   stop semantics as the unsafe-path case.
///
/// Permission bits from each tar header are applied via
/// `set_permissions` on the still-open fd of the just-written
/// file (Unix only; Windows ignores them, matching the standard
/// `tar` crate's behaviour). Mtimes and ownership are not
/// touched -- the standard `tar::Archive::unpack` only restores
/// them when `preserve_*` flags are flipped, which rattler does
/// not do.
pub(crate) fn unpack_safely<R: Read>(
    archive: &mut tar::Archive<R>,
    destination: &Path,
) -> Result<(), ExtractError> {
    let dest_dir = Dir::open_ambient_dir(destination, ambient_authority())
        .map_err(ExtractError::CouldNotCreateDestination)?;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        if rattler_fs_safety::validate_relative_inside(destination, &entry_path).is_err() {
            return Err(ExtractError::UnsafeArchivePath(entry_path));
        }

        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Continuous => {
                ensure_parent(&dest_dir, &entry_path)?;
                let mut out = dest_dir.create(&entry_path)?;
                std::io::copy(&mut entry, &mut out)?;
                #[cfg(unix)]
                if let Ok(mode) = entry.header().mode() {
                    out.set_permissions(Permissions::from_mode(mode))?;
                }
            }
            EntryType::Directory => {
                dest_dir.create_dir_all(&entry_path)?;
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()?
                    .ok_or_else(|| ExtractError::UnsafeArchivePath(entry_path.clone()))?
                    .into_owned();
                // The symlink itself lives at `<dest>/<entry_path>`;
                // a relative target resolves against its parent
                // directory. Reject anything that, after lexical
                // resolution, escapes `destination`.
                let symlink_parent = entry_path.parent().unwrap_or_else(|| Path::new(""));
                let resolved = symlink_parent.join(&target);
                if rattler_fs_safety::validate_relative_inside(destination, &resolved).is_err() {
                    return Err(ExtractError::UnsafeArchivePath(entry_path));
                }
                ensure_parent(&dest_dir, &entry_path)?;
                create_symlink(&dest_dir, &target, &entry_path)?;
            }
            EntryType::Link => {
                // Hard link. The target is another path within the
                // archive; it must lexically stay inside the
                // destination, and it must already have been
                // extracted (tar enforces this ordering by writing
                // the target entry before any hardlinks pointing
                // at it).
                let target = entry
                    .link_name()?
                    .ok_or_else(|| ExtractError::UnsafeArchivePath(entry_path.clone()))?
                    .into_owned();
                if rattler_fs_safety::validate_relative_inside(destination, &target).is_err() {
                    return Err(ExtractError::UnsafeArchivePath(entry_path));
                }
                ensure_parent(&dest_dir, &entry_path)?;
                dest_dir.hard_link(&target, &dest_dir, &entry_path)?;
            }
            // Skip device files, FIFOs, char/block specials, GNU
            // extensions we don't model. `tar::Archive::unpack`
            // skips these too; rattler has never supported them.
            _ => {}
        }
    }
    Ok(())
}

/// Create the parent of `entry_path` (relative to `dest_dir`) if
/// it doesn't already exist. No-op when `entry_path` has no
/// parent or its parent is the destination directory itself.
fn ensure_parent(dest_dir: &Dir, entry_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = entry_path.parent() {
        if !parent.as_os_str().is_empty() {
            dest_dir.create_dir_all(parent)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(dest_dir: &Dir, target: &Path, link: &Path) -> std::io::Result<()> {
    dest_dir.symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(dest_dir: &Dir, target: &Path, link: &Path) -> std::io::Result<()> {
    // Windows distinguishes file vs directory symlinks at create
    // time. The tar header doesn't tell us which the target is --
    // try the file variant first, fall back to the directory
    // variant on failure. Matches `tar::Archive::unpack`'s
    // best-effort handling.
    dest_dir
        .symlink_file(target, link)
        .or_else(|_| dest_dir.symlink_dir(target, link))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a minimal tar archive whose first entry is a regular
    /// file at `entry_path`. We hand-craft the 512-byte ustar
    /// header instead of going through `tar::Builder` because the
    /// builder refuses to write paths containing `..` or absolute
    /// paths -- exactly the inputs we need to construct to verify
    /// that the *reader* refuses them too.
    fn tar_with_regular_entry(entry_path: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        // name (offset 0, 100 bytes)
        let name_bytes = entry_path.as_bytes();
        header[..name_bytes.len()].copy_from_slice(name_bytes);
        // mode (offset 100, 8 bytes, octal ASCII + NUL)
        header[100..108].copy_from_slice(b"0000644\0");
        // uid (108, 8) / gid (116, 8) -- zeros
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        // size (124, 12, octal ASCII + NUL)
        let size_str = format!("{:011o}\0", contents.len());
        header[124..136].copy_from_slice(size_str.as_bytes());
        // mtime (136, 12)
        header[136..148].copy_from_slice(b"00000000000\0");
        // typeflag (156) -- '0' is regular file
        header[156] = b'0';
        // ustar magic (257, 6) + version (263, 2)
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // checksum (148, 8) -- sum of all bytes treating cksum
        // field as 8 spaces.
        for b in &mut header[148..156] {
            *b = b' ';
        }
        let cksum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        let cksum_str = format!("{cksum:06o}\0 ");
        header[148..156].copy_from_slice(cksum_str.as_bytes());

        let mut buf = header.to_vec();
        buf.extend_from_slice(contents);
        // pad data to 512-byte block
        let pad = (512 - (contents.len() % 512)) % 512;
        buf.extend(std::iter::repeat_n(0u8, pad));
        // two blocks of zero terminate the archive
        buf.extend(std::iter::repeat_n(0u8, 1024));
        buf
    }

    /// Build an in-memory tar with a single symlink entry whose
    /// target is `link_target`.
    fn tar_with_symlink(link_path: &str, link_target: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_link_name(link_target).unwrap();
            header.set_cksum();
            builder
                .append_data(&mut header, link_path, std::io::empty())
                .unwrap();
            builder.finish().unwrap();
        }
        buf
    }

    #[test]
    fn rejects_parent_dir_escape() {
        let dest = tempfile::tempdir().unwrap();
        let bytes = tar_with_regular_entry("../escaped.txt", b"x");
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let err = unpack_safely(&mut archive, dest.path()).unwrap_err();
        assert!(matches!(err, ExtractError::UnsafeArchivePath(_)), "{err:?}");
    }

    #[test]
    fn rejects_absolute_path_entry() {
        let dest = tempfile::tempdir().unwrap();
        let bytes = tar_with_regular_entry("/etc/passwd", b"x");
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let err = unpack_safely(&mut archive, dest.path()).unwrap_err();
        assert!(matches!(err, ExtractError::UnsafeArchivePath(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_pointing_outside() {
        let dest = tempfile::tempdir().unwrap();
        let bytes = tar_with_symlink("link", "../../etc");
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let err = unpack_safely(&mut archive, dest.path()).unwrap_err();
        assert!(matches!(err, ExtractError::UnsafeArchivePath(_)), "{err:?}");
    }

    /// Symlinks within the destination are allowed -- they don't
    /// escape it, and conda packages legitimately use them.
    #[cfg(unix)]
    #[test]
    fn allows_in_dir_symlink() {
        let dest = tempfile::tempdir().unwrap();
        let bytes = tar_with_symlink("a/link", "real");
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        unpack_safely(&mut archive, dest.path()).unwrap();
        assert!(dest.path().join("a/link").is_symlink());
    }

    #[test]
    fn extracts_regular_file_through_dir_capability() {
        let dest = tempfile::tempdir().unwrap();
        let bytes = tar_with_regular_entry("nested/dir/file.txt", b"hi");
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        unpack_safely(&mut archive, dest.path()).unwrap();
        assert_eq!(
            std::fs::read(dest.path().join("nested/dir/file.txt")).unwrap(),
            b"hi"
        );
    }
}
