//! Caching what a virtual package detection plugin reported.
//!
//! Running a plugin means installing an environment and starting a process, so
//! the verdicts are worth keeping.
//!
//! The cache deliberately knows nothing about the plugin protocol: the caller
//! turns a plugin's declared policy into an expiry and a set of things to watch.

mod cache_key;

use std::path::{Path, PathBuf};

pub use cache_key::CacheKey;
use rattler_conda_types::SourcedVirtualPackage;
use serde::{Deserialize, Serialize};

/// The state of a watched path when the verdicts were recorded.
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

/// The value of a watched environment variable when the verdicts were recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedEnv {
    /// The variable that was watched.
    pub name: String,

    /// What it held, or `None` if it was not set.
    pub value: Option<String>,
}

impl WatchedEnv {
    /// Records the current value of `name`.
    pub fn record(name: impl Into<String>) -> Self {
        Self::record_from(name, environment_value)
    }

    /// Whether the variable still holds what it did when it was recorded.
    pub fn is_unchanged(&self) -> bool {
        self.is_unchanged_from(environment_value)
    }

    /// [`WatchedEnv::record`] against something other than the process
    /// environment.
    fn record_from(name: impl Into<String>, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let name = name.into();
        Self {
            value: lookup(&name),
            name,
        }
    }

    /// [`WatchedEnv::is_unchanged`] against something other than the process
    /// environment.
    fn is_unchanged_from(&self, lookup: impl Fn(&str) -> Option<String>) -> bool {
        lookup(&self.name) == self.value
    }
}

/// What the process environment holds for `name`, or `None` if it is unset.
fn environment_value(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// What a plugin asked to have watched on its behalf.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchList {
    /// Paths whose existence or modification time matters.
    pub paths: Vec<PathBuf>,

    /// Environment variables whose value or absence matters. These are read
    /// from the process that runs the plugin, not from the plugin's activated
    /// environment.
    pub env: Vec<String>,
}

/// Verdicts from one plugin run, with what makes them stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedDetection {
    /// What the plugin reported, ready to hand to a solve.
    pub virtual_packages: Vec<SourcedVirtualPackage>,

    /// When these verdicts stop being usable, in seconds since the Unix epoch.
    /// `None` means no time limit, so only `watched` can invalidate them.
    pub expires_at: Option<i64>,

    /// Paths the plugin asked to have watched, as they were at record time.
    #[serde(default)]
    pub watched: Vec<WatchedPath>,

    /// Environment variables the plugin asked to have watched, as they were at
    /// record time.
    #[serde(default)]
    pub watched_env: Vec<WatchedEnv>,
}

impl CachedDetection {
    /// Records verdicts that expire `ttl_seconds` after `now`, watching what
    /// `watch` names as it is right now.
    pub fn record(
        virtual_packages: Vec<SourcedVirtualPackage>,
        ttl_seconds: Option<u64>,
        watch: &WatchList,
        now: i64,
    ) -> Self {
        Self {
            virtual_packages,
            expires_at: ttl_seconds
                .and_then(|ttl| i64::try_from(ttl).ok())
                .and_then(|ttl| now.checked_add(ttl)),
            watched: watch
                .paths
                .iter()
                .cloned()
                .map(WatchedPath::record)
                .collect(),
            watched_env: watch.env.iter().map(WatchedEnv::record).collect(),
        }
    }

    /// Whether these verdicts may still be used at `now`.
    pub fn is_valid(&self, now: i64) -> bool {
        if self.expires_at.is_some_and(|expires_at| now >= expires_at) {
            return false;
        }
        self.watched.iter().all(WatchedPath::is_unchanged)
            && self.watched_env.iter().all(WatchedEnv::is_unchanged)
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
    /// A missing, unreadable or stale entry is a miss rather than an error.
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
    ///
    /// The entry is written beside its destination and renamed over it, so a
    /// concurrent reader sees either the whole previous entry or the whole new
    /// one. Several processes share one cache directory and rewrite the same
    /// keys, and an interrupted write must not leave a half-entry behind.
    pub fn put(&self, key: &CacheKey, detection: &CachedDetection) -> Result<(), CacheError> {
        let path = self.root.join(key.to_file_name()?);
        fs_err::create_dir_all(&self.root)?;

        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec(detection)?)?;
        file.persist(path)
            .map_err(|err| CacheError::Io(err.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rattler_conda_types::{GenericVirtualPackage, PackageName, VirtualPackageSource};
    use rattler_digest::{Sha256, compute_bytes_digest};

    use super::*;

    fn key() -> CacheKey {
        CacheKey::new(
            url::Url::parse("https://prefix.dev/org/").unwrap().into(),
            PackageName::new_unchecked("cuda-detect"),
            compute_bytes_digest::<Sha256>([1]),
        )
    }

    /// A watch list covering one path and nothing else.
    fn watching(path: &Path) -> WatchList {
        WatchList {
            paths: vec![path.to_path_buf()],
            ..WatchList::default()
        }
    }

    fn detected() -> Vec<SourcedVirtualPackage> {
        vec![SourcedVirtualPackage {
            source: VirtualPackageSource::Plugin {
                channel: url::Url::parse("https://prefix.dev/org/").unwrap().into(),
                plugin: PackageName::new_unchecked("cuda-detect"),
                environment: compute_bytes_digest::<Sha256>([1]),
            },
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
        let recorded = CachedDetection::record(detected(), Some(60), &WatchList::default(), 1_000);

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
                &CachedDetection::record(detected(), Some(60), &WatchList::default(), 1_000),
            )
            .unwrap();

        assert!(cache.get(&key(), 1_059).unwrap().is_some(), "still fresh");
        assert!(cache.get(&key(), 1_060).unwrap().is_none(), "ttl elapsed");
    }

    #[test]
    fn without_a_ttl_an_entry_does_not_expire() {
        let recorded = CachedDetection::record(detected(), None, &WatchList::default(), 0);
        assert!(recorded.is_valid(i64::MAX));
    }

    #[test]
    fn a_changed_watched_path_invalidates_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("version");
        fs_err::write(&watched, "6.1.2").unwrap();

        let recorded = CachedDetection::record(detected(), None, &watching(&watched), 0);
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

        let recorded_absent = CachedDetection::record(detected(), None, &watching(&watched), 0);
        assert!(recorded_absent.is_valid(0));
        fs_err::write(&watched, "6.1.2").unwrap();
        assert!(
            !recorded_absent.is_valid(0),
            "a driver appearing must invalidate"
        );

        let recorded_present = CachedDetection::record(detected(), None, &watching(&watched), 0);
        assert!(recorded_present.is_valid(0));
        fs_err::remove_file(&watched).unwrap();
        assert!(
            !recorded_present.is_valid(0),
            "a driver being removed must invalidate"
        );
    }

    #[test]
    fn a_changed_watched_environment_variable_invalidates_the_entry() {
        let unset = |_: &str| None;
        let hidden = |_: &str| Some("0".to_string());
        let shown = |_: &str| Some("1".to_string());

        // Recorded while unset: it appearing has to invalidate.
        let recorded_unset = WatchedEnv::record_from("CUDA_VISIBLE_DEVICES", unset);
        assert!(recorded_unset.is_unchanged_from(unset));
        assert!(
            !recorded_unset.is_unchanged_from(hidden),
            "a variable appearing must invalidate"
        );

        // Recorded while set: changing it, and unsetting it, both have to.
        let recorded_set = WatchedEnv::record_from("CUDA_VISIBLE_DEVICES", hidden);
        assert!(recorded_set.is_unchanged_from(hidden));
        assert!(
            !recorded_set.is_unchanged_from(shown),
            "a changed variable must invalidate"
        );
        assert!(
            !recorded_set.is_unchanged_from(unset),
            "a variable being unset must invalidate"
        );
    }

    #[test]
    fn watched_variables_are_recorded_with_the_entry() {
        let watch = WatchList {
            env: vec!["PATH".to_string()],
            ..WatchList::default()
        };
        let recorded = CachedDetection::record(detected(), None, &watch, 0);

        assert_eq!(recorded.watched_env.len(), 1);
        assert_eq!(recorded.watched_env[0].name, "PATH");
        assert!(
            recorded.is_valid(0),
            "nothing has changed since it was recorded"
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

    /// A reader must never miss an entry that stays valid throughout, no matter
    /// how often it is rewritten underneath: concurrent pixi invocations share
    /// one cache directory and rewrite the same keys.
    #[test]
    fn rewriting_an_entry_never_makes_a_reader_miss() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Large enough that a write cannot land in a single operation.
        let recorded = CachedDetection::record(
            (0..400)
                .map(|index| SourcedVirtualPackage {
                    source: VirtualPackageSource::Plugin {
                        channel: url::Url::parse("https://prefix.dev/org/").unwrap().into(),
                        plugin: PackageName::new_unchecked("cuda-detect"),
                        environment: compute_bytes_digest::<Sha256>([1]),
                    },
                    package: GenericVirtualPackage {
                        name: PackageName::new_unchecked("__cuda"),
                        version: format!("12.{index}").parse().unwrap(),
                        build_string: String::new(),
                    },
                })
                .collect(),
            None,
            &WatchList::default(),
            0,
        );
        VirtualPackagePluginCache::new(&root)
            .put(&key(), &recorded)
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let misses = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(AtomicUsize::new(0));

        let writers: Vec<_> = (0..4)
            .map(|_| {
                let (root, stop, recorded) = (root.clone(), stop.clone(), recorded.clone());
                std::thread::spawn(move || {
                    let cache = VirtualPackagePluginCache::new(&root);
                    while !stop.load(Ordering::Relaxed) {
                        cache.put(&key(), &recorded).unwrap();
                    }
                })
            })
            .collect();

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let (root, misses, errors) = (root.clone(), misses.clone(), errors.clone());
                std::thread::spawn(move || {
                    let cache = VirtualPackagePluginCache::new(&root);
                    for _ in 0..800 {
                        match cache.get(&key(), 0) {
                            Ok(Some(_)) => {}
                            Ok(None) => {
                                misses.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                })
            })
            .collect();

        for reader in readers {
            reader.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        for writer in writers {
            writer.join().unwrap();
        }

        assert_eq!(
            (
                misses.load(Ordering::Relaxed),
                errors.load(Ordering::Relaxed)
            ),
            (0, 0),
            "an entry that was valid the whole time read back as a miss or an error"
        );
    }
}
