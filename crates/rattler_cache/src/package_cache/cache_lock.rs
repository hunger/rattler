use std::{
    fmt::{Debug, Formatter},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use digest::generic_array::GenericArray;
use fs4::fs_std::FileExt;
use rattler_conda_types::package::{IndexJson, PathsJson};
use rattler_digest::Sha256Hash;

use crate::package_cache::PackageCacheLayerError;

/// A validated cache entry with its associated metadata.
///
/// This struct represents a cache entry that has been validated and is ready for use.
/// It holds the cache entry's path, revision number, and optional SHA256 hash.
///
/// Concurrent access is serialised by [`CacheMetadataFile`], which
/// takes an exclusive `flock` on the entry's `.lock` file for the
/// duration of validate-or-fetch. A [`CacheGlobalLock`] can
/// additionally be held to amortise overhead across many entries.
pub struct CacheMetadata {
    pub(super) revision: u64,
    pub(super) sha256: Option<Sha256Hash>,
    pub(super) path: PathBuf,
    pub(super) index_json: Option<IndexJson>,
    pub(super) paths_json: Option<PathsJson>,
}

impl Debug for CacheMetadata {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheMetadata")
            .field("path", &self.path)
            .field("revision", &self.revision)
            .field("sha256", &self.sha256)
            .finish()
    }
}

impl CacheMetadata {
    /// Returns the path to the cache entry on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the revision of the cache entry. This revision indicates the
    /// number of times the cache entry has been updated.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the cached `index.json` data if it was read during validation.
    pub fn index_json(&self) -> Option<&IndexJson> {
        self.index_json.as_ref()
    }

    /// Returns the cached `paths.json` data if it was read during validation.
    pub fn paths_json(&self) -> Option<&PathsJson> {
        self.paths_json.as_ref()
    }
}

/// A coarse advisory lock covering the entire package cache.
///
/// Per-entry locking is handled unconditionally by
/// [`CacheMetadataFile`], so the global lock is **not** required
/// for correctness against concurrent writers. It remains useful
/// when a caller wants a single point of mutual exclusion across
/// many entries -- for instance during a cache-wide maintenance
/// pass -- without taking per-entry locks one by one as it walks.
pub struct CacheGlobalLock {
    file: std::fs::File,
}

impl Debug for CacheGlobalLock {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheGlobalLock").finish()
    }
}

impl Drop for CacheGlobalLock {
    fn drop(&mut self) {
        // Ensure that the lock is released when the lock is dropped.
        let _ = fs4::fs_std::FileExt::unlock(&self.file);
    }
}

impl CacheGlobalLock {
    /// Acquires a global write lock on the package cache.
    ///
    /// This lock should be used to coordinate access across multiple package
    /// operations to reduce the overhead of acquiring individual locks.
    pub async fn acquire(path: &Path) -> Result<Self, PackageCacheLayerError> {
        let lock_file_path = path.to_path_buf();
        let acquire_lock_fut = simple_spawn_blocking::tokio::run_blocking_task(move || {
            let file = open_lock_file_no_follow(&lock_file_path).map_err(|e| {
                PackageCacheLayerError::LockError(
                    format!(
                        "failed to open global cache lock for writing: '{}'",
                        lock_file_path.display()
                    ),
                    e,
                )
            })?;

            file.lock_exclusive().map_err(move |e| {
                PackageCacheLayerError::LockError(
                    format!(
                        "failed to acquire write lock on global cache lock file: '{}'",
                        lock_file_path.display()
                    ),
                    e,
                )
            })?;

            Ok(CacheGlobalLock { file })
        });

        tokio::select!(
            lock = acquire_lock_fut => lock,
            _ = warn_timeout_future(
                "Blocking waiting for global file lock on package cache".to_string()
            ) => unreachable!("warn_timeout_future should never finish")
        )
    }
}

/// A handle to a cache metadata file.
///
/// This struct manages access to a `.lock` file that stores
/// metadata about a cache entry -- its revision number and
/// optional SHA256 hash. `acquire` takes an **exclusive
/// `flock`** on the file before returning, so concurrent
/// `PackageCache` instances in the same process (or separate
/// processes) sharing the same cache root serialize on this lock
/// rather than racing each other through `validate_package_common`.
/// The lock is released when the file is dropped (the kernel
/// releases all advisory locks held by an fd when it's closed).
pub struct CacheMetadataFile {
    file: Arc<std::fs::File>,
}

impl CacheMetadataFile {
    /// Acquires the cache metadata file *and* takes an exclusive
    /// `flock` on it.
    ///
    /// Two callers asking for the same entry's metadata file run
    /// sequentially: the second one blocks inside `lock_exclusive`
    /// until the first drops its handle. Different entries use
    /// different lock files, so they don't contend.
    ///
    /// The optional [`CacheGlobalLock`] is still useful when a
    /// caller wants to amortise locking overhead across many
    /// per-entry operations, but it's no longer required for
    /// correctness against concurrent writers -- this per-entry
    /// `flock` is the new floor.
    pub async fn acquire(path: &Path) -> Result<Self, PackageCacheLayerError> {
        let lock_file_path = path.to_path_buf();

        simple_spawn_blocking::tokio::run_blocking_task(move || {
            let file = open_lock_file_no_follow(&lock_file_path).map_err(|e| {
                PackageCacheLayerError::LockError(
                    format!(
                        "failed to open cache metadata file: '{}'",
                        lock_file_path.display()
                    ),
                    e,
                )
            })?;

            file.lock_exclusive().map_err(|e| {
                PackageCacheLayerError::LockError(
                    format!(
                        "failed to acquire exclusive lock on cache metadata file: '{}'",
                        lock_file_path.display()
                    ),
                    e,
                )
            })?;

            Ok(CacheMetadataFile {
                file: Arc::new(file),
            })
        })
        .await
    }
}

impl Drop for CacheMetadataFile {
    fn drop(&mut self) {
        // Best-effort `flock` release. The kernel will release
        // the advisory lock anyway when the fd is closed (which
        // happens when the last `Arc<File>` is dropped), but
        // doing it explicitly here makes the lock lifetime
        // obvious to readers and lets other waiters proceed as
        // soon as we're done rather than at fd-close time.
        let _ = fs4::fs_std::FileExt::unlock(&*self.file);
    }
}

impl CacheMetadataFile {
    pub async fn write_revision_and_sha(
        &mut self,
        revision: u64,
        sha256: Option<&Sha256Hash>,
    ) -> Result<(), PackageCacheLayerError> {
        let file = self.file.clone();

        let sha256 = sha256.cloned();
        simple_spawn_blocking::tokio::run_blocking_task(move || {
            // Ensure we write from the start of the file
            (&*file).rewind().map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to rewind cache lock for reading revision".to_string(),
                    e,
                )
            })?;

            // Write the bytes of the revision
            let revision_bytes = revision.to_be_bytes();
            (&*file).write_all(&revision_bytes).map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to write revision from cache lock".to_string(),
                    e,
                )
            })?;

            // Write the bytes of the sha256 hash
            let sha_bytes = if let Some(sha) = sha256 {
                let len = sha.len();
                let sha = &sha[..];
                (&*file).write_all(sha).map_err(|e| {
                    PackageCacheLayerError::LockError(
                        "failed to write sha256 from cache lock".to_string(),
                        e,
                    )
                })?;
                len
            } else {
                0
            };

            // Ensure all bytes are written to disk
            (&*file).flush().map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to flush cache lock after writing revision".to_string(),
                    e,
                )
            })?;

            // Update the length of the file
            let file_length = revision_bytes.len() + sha_bytes;
            file.set_len(file_length as u64).map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to truncate cache lock after writing revision".to_string(),
                    e,
                )
            })?;

            Ok(())
        })
        .await
    }

    /// Reads the revision from the cache metadata file.
    pub fn read_revision(&mut self) -> Result<u64, PackageCacheLayerError> {
        (&*self.file).rewind().map_err(|e| {
            PackageCacheLayerError::LockError(
                "failed to rewind cache lock for reading revision".to_string(),
                e,
            )
        })?;
        let mut buf = [0; 8];
        match (&*self.file).read_exact(&mut buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(0);
            }
            Err(e) => {
                return Err(PackageCacheLayerError::LockError(
                    "failed to read revision from cache lock".to_string(),
                    e,
                ));
            }
        }
        Ok(u64::from_be_bytes(buf))
    }

    /// Reads the sha256 hash from the cache metadata file.
    pub fn read_sha256(&mut self) -> Result<Option<Sha256Hash>, PackageCacheLayerError> {
        const SHA256_LEN: usize = 32;
        const REVISION_LEN: u64 = 8;
        (&*self.file).rewind().map_err(|e| {
            PackageCacheLayerError::LockError(
                "failed to rewind cache lock for reading sha256".to_string(),
                e,
            )
        })?;
        let mut buf = [0; SHA256_LEN];
        let _ = (&*self.file)
            .seek(SeekFrom::Start(REVISION_LEN))
            .map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to seek to sha256 in cache lock".to_string(),
                    e,
                )
            })?;
        match (&*self.file).read_exact(&mut buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => {
                return Err(PackageCacheLayerError::LockError(
                    "failed to read sha256 from cache lock".to_string(),
                    e,
                ));
            }
        }
        Ok(Some(GenericArray::clone_from_slice(&buf)))
    }
}

async fn warn_timeout_future(message: String) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        tracing::warn!("{}", &message);
    }
}

/// Open `lock_file_path` with `O_NOFOLLOW` semantics so a
/// preplanted symlink at the final component can't redirect the
/// open at a sensitive file. The cache root may be shared with
/// untrusted local users; without this guard, a co-tenant who
/// placed `<entry>.lock` as a symlink to `~/.bashrc` could have
/// `CacheMetadataFile::write_revision_and_sha` truncate-and-
/// overwrite that target with binary revision/sha bytes.
///
/// Routed through `rattler_fs_safety::open_no_follow` which uses
/// a `cap_std::fs::Dir` capability on every platform rattler
/// supports (cross-platform symlink-refusal cheaper than a
/// Unix-only `OpenOptionsExt::custom_flags(O_NOFOLLOW)`).
fn open_lock_file_no_follow(lock_file_path: &Path) -> std::io::Result<std::fs::File> {
    let parent = lock_file_path.parent().ok_or_else(|| {
        std::io::Error::other(format!(
            "lock file path has no parent: {}",
            lock_file_path.display()
        ))
    })?;
    let name = lock_file_path.file_name().ok_or_else(|| {
        std::io::Error::other(format!(
            "lock file path has no final component: {}",
            lock_file_path.display()
        ))
    })?;
    let cap_file = rattler_fs_safety::open_no_follow(
        parent,
        name,
        rattler_fs_safety::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true),
    )?;
    Ok(cap_file.into_std())
}

#[cfg(test)]
mod tests {
    use rattler_digest::{parse_digest_from_hex, Sha256};

    use super::CacheMetadataFile;

    #[tokio::test]
    async fn cache_metadata_serialize_deserialize() {
        // Temporarily create a metadata file and write a revision and sha to it
        let temp_dir = tempfile::tempdir().unwrap();
        let metadata_file = temp_dir.path().join("foo.lock");
        // Acquire a handle on the file
        let mut metadata = CacheMetadataFile::acquire(&metadata_file).await.unwrap();
        // Write a revision and sha to the lock file
        let sha = parse_digest_from_hex::<Sha256>(
            "4dd9893f1eee45e1579d1a4f5533ef67a84b5e4b7515de7ed0db1dd47adc6bc8",
        );
        metadata
            .write_revision_and_sha(1, sha.as_ref())
            .await
            .unwrap();
        // Read back the revision and sha from the metadata file
        let revision = metadata.read_revision().unwrap();
        assert_eq!(revision, 1);
        let read_sha = metadata.read_sha256().unwrap();
        assert_eq!(sha, read_sha);
    }

    /// Two concurrent `CacheMetadataFile::acquire` calls on the
    /// same path must serialise: the second one blocks until the
    /// first handle is dropped. Deleting the `lock_exclusive` call
    /// in `acquire` makes this test fail (the second handle
    /// returns immediately while the first is still alive).
    #[tokio::test]
    async fn acquire_serialises_concurrent_callers() {
        use std::time::Duration;

        let temp_dir = tempfile::tempdir().unwrap();
        let metadata_file = temp_dir.path().join("foo.lock");

        let first = CacheMetadataFile::acquire(&metadata_file).await.unwrap();

        let path_for_second = metadata_file.clone();
        let second_task =
            tokio::spawn(async move { CacheMetadataFile::acquire(&path_for_second).await });

        // Give the second task a generous chance to reach the
        // blocking `flock` call. If the lock weren't held the
        // second acquire would complete within a few ms.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !second_task.is_finished(),
            "second acquire should still be blocked while the first handle is live"
        );

        drop(first);

        let second = tokio::time::timeout(Duration::from_secs(5), second_task)
            .await
            .expect("second acquire did not complete after the first handle was dropped")
            .expect("second-acquire task panicked")
            .expect("second acquire returned an error");
        drop(second);
    }
}
