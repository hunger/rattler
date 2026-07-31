//! Caching what a virtual package detection plugin reported.
//!
//! Running a plugin means installing an environment and starting a process, so
//! the verdicts are worth keeping. Two things can make a kept verdict wrong:
//! time passing, and the system changing underneath it. A plugin says which of
//! those apply to it, and this module stores enough to check both.
//!
//! The cache deliberately knows nothing about the plugin protocol: the caller
//! turns a plugin's declared policy into an expiry and a set of watched paths,
//! and what is stored here is those facts.

mod cache_key;

use std::path::{Path, PathBuf};

pub use cache_key::CacheKey;
use rattler_conda_types::ChannelVirtualPackage;
use serde::{Deserialize, Serialize};

/// The state of a watched path when the verdicts were recorded.
///
/// Existence is part of it: a driver appearing is as much a change as one being
/// upgraded, and a plugin watching `/sys/module/amdgpu/version` cares about
/// exactly that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedPath {
    /// The path that was watched.
    pub path: PathBuf,

    /// Modification time in milliseconds since the Unix epoch, or `None` if the
    /// path did not exist.
    pub modified_ms: Option<i64>,
}

impl WatchedPath {
    /// Records the current state of `path`.
    pub fn record(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            modified_ms: modified_ms(&path),
            path,
        }
    }

    /// Whether the path still looks the way it did when it was recorded.
    pub fn is_unchanged(&self) -> bool {
        modified_ms(&self.path) == self.modified_ms
    }
}

/// The modification time of `path` in milliseconds since the Unix epoch, or
/// `None` if it does not exist or has no readable timestamp.
fn modified_ms(path: &Path) -> Option<i64> {
    let modified = fs_err::metadata(path).ok()?.modified().ok()?;
    let since_epoch = modified
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?;
    i64::try_from(since_epoch.as_millis()).ok()
}

/// Verdicts from one plugin run, with what makes them stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedDetection {
    /// What the plugin reported, ready to hand to a solve.
    pub virtual_packages: Vec<ChannelVirtualPackage>,

    /// When these verdicts stop being usable, in seconds since the Unix epoch.
    /// `None` means no time limit, so only `watched` can invalidate them.
    pub expires_at: Option<i64>,

    /// Paths the plugin asked to have watched, as they were at record time.
    #[serde(default)]
    pub watched: Vec<WatchedPath>,
}

impl CachedDetection {
    /// Records verdicts that expire `ttl_seconds` after `now`, watching
    /// `watch_paths` as they are right now.
    ///
    /// `now` is a parameter rather than read from the clock so expiry is
    /// testable without waiting.
    pub fn record(
        virtual_packages: Vec<ChannelVirtualPackage>,
        ttl_seconds: Option<u64>,
        watch_paths: impl IntoIterator<Item = PathBuf>,
        now: i64,
    ) -> Self {
        Self {
            virtual_packages,
            expires_at: ttl_seconds
                .and_then(|ttl| i64::try_from(ttl).ok())
                .and_then(|ttl| now.checked_add(ttl)),
            watched: watch_paths.into_iter().map(WatchedPath::record).collect(),
        }
    }

    /// Whether these verdicts may still be used at `now`.
    pub fn is_valid(&self, now: i64) -> bool {
        if self.expires_at.is_some_and(|expires_at| now >= expires_at) {
            return false;
        }
        self.watched.iter().all(WatchedPath::is_unchanged)
    }
}

/// Reading and writing plugin verdicts under a cache directory.
#[derive(Debug, Clone)]
pub struct VirtualPackagePluginCache {
    root: PathBuf,
}

/// Something went wrong reading or writing the cache.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The key could not be turned into a file name.
    #[error(transparent)]
    InvalidKey(#[from] rattler_conda_types::utils::InvalidPathComponentError),

    /// The cache directory or entry could not be read or written.
    #[error("failed to access the virtual package plugin cache")]
    Io(#[from] std::io::Error),

    /// An entry was unreadable. Treated as a miss by [`VirtualPackagePluginCache::get`].
    #[error("failed to parse a cached detection")]
    Corrupt(#[from] serde_json::Error),
}

impl VirtualPackagePluginCache {
    /// Stores entries under `root`, which is created on first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The verdicts for `key` if they are present and still valid at `now`.
    ///
    /// A missing, unreadable or stale entry is a miss rather than an error: the
    /// only cost of a miss is running the plugin again, whereas failing the solve
    /// over a corrupt cache file would be worse than useless.
    pub fn get(&self, key: &CacheKey, now: i64) -> Result<Option<CachedDetection>, CacheError> {
        let path = self.root.join(key.to_file_name()?);
        let bytes = match fs_err::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };

        let Ok(detection) = serde_json::from_slice::<CachedDetection>(&bytes) else {
            tracing::debug!("ignoring unreadable plugin cache entry at {path:?}");
            return Ok(None);
        };

        Ok(detection.is_valid(now).then_some(detection))
    }

    /// Stores `detection` under `key`, replacing any previous entry.
    pub fn put(&self, key: &CacheKey, detection: &CachedDetection) -> Result<(), CacheError> {
        let path = self.root.join(key.to_file_name()?);
        fs_err::create_dir_all(&self.root)?;
        fs_err::write(path, serde_json::to_vec(detection)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rattler_conda_types::{GenericVirtualPackage, PackageName};
    use rattler_digest::{Sha256, compute_bytes_digest};

    use super::*;

    fn key() -> CacheKey {
        CacheKey::new(
            url::Url::parse("https://prefix.dev/org/").unwrap().into(),
            PackageName::new_unchecked("cuda-detect"),
            compute_bytes_digest::<Sha256>([1]),
        )
    }

    fn detected() -> Vec<ChannelVirtualPackage> {
        vec![ChannelVirtualPackage {
            channel: url::Url::parse("https://prefix.dev/org/").unwrap().into(),
            plugin_sha256: compute_bytes_digest::<Sha256>([1]),
            package: GenericVirtualPackage {
                name: PackageName::new_unchecked("__cuda"),
                version: "12.4".parse().unwrap(),
                build_string: String::new(),
            },
        }]
    }

    #[test]
    fn round_trips_verdicts() {
        let dir = tempfile::tempdir().unwrap();
        let cache = VirtualPackagePluginCache::new(dir.path());
        let recorded = CachedDetection::record(detected(), Some(60), [], 1_000);

        cache.put(&key(), &recorded).unwrap();
        assert_eq!(cache.get(&key(), 1_000).unwrap(), Some(recorded));
    }

    #[test]
    fn an_absent_entry_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = VirtualPackagePluginCache::new(dir.path());
        assert_eq!(cache.get(&key(), 0).unwrap(), None);
    }

    #[test]
    fn an_expired_entry_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = VirtualPackagePluginCache::new(dir.path());
        cache
            .put(
                &key(),
                &CachedDetection::record(detected(), Some(60), [], 1_000),
            )
            .unwrap();

        assert!(cache.get(&key(), 1_059).unwrap().is_some(), "still fresh");
        assert!(cache.get(&key(), 1_060).unwrap().is_none(), "ttl elapsed");
    }

    #[test]
    fn without_a_ttl_an_entry_does_not_expire() {
        let recorded = CachedDetection::record(detected(), None, [], 0);
        assert!(recorded.is_valid(i64::MAX));
    }

    /// The case a TTL cannot catch: the driver changed, so the verdicts are wrong
    /// however recently they were taken.
    #[test]
    fn a_changed_watched_path_invalidates_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("version");
        fs_err::write(&watched, "6.1.2").unwrap();

        let recorded = CachedDetection::record(detected(), None, [watched.clone()], 0);
        assert!(recorded.is_valid(0));

        // A different modification time is what the check keys on, so set one
        // explicitly rather than relying on filesystem timestamp resolution.
        let later = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs_err::write(&watched, "6.2.0").unwrap();
        filetime::set_file_mtime(&watched, filetime::FileTime::from_system_time(later)).unwrap();
        assert!(!recorded.is_valid(0), "an upgraded driver must invalidate");
    }

    #[test]
    fn a_watched_path_appearing_or_vanishing_invalidates_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("version");

        let recorded_absent = CachedDetection::record(detected(), None, [watched.clone()], 0);
        assert!(recorded_absent.is_valid(0));
        fs_err::write(&watched, "6.1.2").unwrap();
        assert!(
            !recorded_absent.is_valid(0),
            "a driver appearing must invalidate"
        );

        let recorded_present = CachedDetection::record(detected(), None, [watched.clone()], 0);
        assert!(recorded_present.is_valid(0));
        fs_err::remove_file(&watched).unwrap();
        assert!(
            !recorded_present.is_valid(0),
            "a driver being removed must invalidate"
        );
    }

    #[test]
    fn a_corrupt_entry_is_a_miss_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = VirtualPackagePluginCache::new(dir.path());
        fs_err::create_dir_all(dir.path()).unwrap();
        fs_err::write(dir.path().join(key().to_file_name().unwrap()), b"{not json").unwrap();

        assert_eq!(cache.get(&key(), 0).unwrap(), None);
    }
}
