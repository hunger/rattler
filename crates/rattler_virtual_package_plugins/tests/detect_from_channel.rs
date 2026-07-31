//! The whole chain against a local channel: read what the channel registers,
//! install the plugin it names, run it, parse what it said, and check that
//! against the registration.
//!
//! This is the seam the unit tests cannot cover. They work from either end --
//! a channel with no package, or a prefix with a hand-written script -- and
//! never exercise a real solve and install in between.

#![cfg(feature = "experimental-virtual-package-plugins")]

use std::path::PathBuf;

use rattler_cache::package_cache::PackageCache;
use rattler_conda_types::{Channel, ChannelConfig, PackageName, Platform};
use rattler_repodata_gateway::Gateway;
use rattler_virtual_package_plugins::{
    PluginEnvironmentOptions, Verdict, ensure_plugin_environment, parse_output, run_plugin,
    validate,
};

/// The fixture channel, which registers `foobar-detect` for `__foobar` and
/// `__foobar_arch` and ships a package providing it.
fn fixture_channel() -> Channel {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let channel_config = ChannelConfig::default_with_root_dir(root.clone());
    Channel::from_str(
        root.join("test-data/channels/virtual-package-plugins")
            .to_string_lossy(),
        &channel_config,
    )
    .expect("the fixture channel path is valid")
}

#[tokio::test]
async fn detects_virtual_packages_from_a_channel_plugin() {
    let cache = tempfile::tempdir().unwrap();
    let channel = fixture_channel();
    let plugin = PackageName::new_unchecked("foobar-detect");
    let platform = Platform::current();

    let gateway = Gateway::builder()
        .with_cache_dir(cache.path().join("repodata"))
        .with_package_cache(PackageCache::new(cache.path().join("pkgs")))
        .finish();

    // What the channel says this plugin speaks for. Everything below is checked
    // against this rather than against a list written into the test.
    let declared: std::collections::BTreeSet<_> = gateway
        .virtual_package_plugins(&channel, platform)
        .await
        .expect("the fixture channel is readable")
        .get(&plugin)
        .expect("the fixture channel registers foobar-detect")
        .iter()
        .cloned()
        .collect();

    let package_cache = PackageCache::new(cache.path().join("pkgs"));
    let environment = ensure_plugin_environment(PluginEnvironmentOptions {
        gateway: &gateway,
        package_cache: &package_cache,
        channel: &channel,
        plugin: &plugin,
        root: &cache.path().join("plugins"),
        host_platform: platform,
    })
    .await
    .expect("the plugin installs from the fixture channel");

    let run = run_plugin(
        &environment.prefix,
        plugin.as_source(),
        platform,
        declared.len(),
    )
    .await
    .expect("the installed entry point runs");
    assert!(
        run.succeeded(),
        "exit {:?}, stderr: {}",
        run.exit_code,
        run.stderr
    );

    let output = parse_output(&run.stdout).expect("the plugin speaks the protocol");
    validate(&declared, &output).expect("the plugin honors what the channel registered");

    let mut detected: Vec<String> = output
        .detections
        .iter()
        .filter_map(Verdict::to_generic)
        .map(|package| package.to_string())
        .collect();
    detected.sort();
    assert_eq!(detected, ["__foobar=1.2.3", "__foobar_arch=0=gen4"]);

    // The plugin declares a cache policy, which is what makes its verdicts
    // reusable.
    assert_eq!(
        output
            .cache_policy
            .expect("declared by the fixture")
            .ttl_seconds,
        Some(3600)
    );
}

/// A second call must reuse the environment rather than reinstall it, and must
/// arrive at the same identity for it.
#[tokio::test]
async fn a_second_call_reuses_the_environment() {
    let cache = tempfile::tempdir().unwrap();
    let channel = fixture_channel();
    let plugin = PackageName::new_unchecked("foobar-detect");
    let platform = Platform::current();

    let gateway = Gateway::builder()
        .with_cache_dir(cache.path().join("repodata"))
        .with_package_cache(PackageCache::new(cache.path().join("pkgs")))
        .finish();
    let package_cache = PackageCache::new(cache.path().join("pkgs"));
    let root = cache.path().join("plugins");

    let options = || PluginEnvironmentOptions {
        gateway: &gateway,
        package_cache: &package_cache,
        channel: &channel,
        plugin: &plugin,
        root: &root,
        host_platform: platform,
    };

    let first = ensure_plugin_environment(options()).await.unwrap();
    let entry_point = first.prefix.join(if platform.is_windows() {
        "Scripts/foobar-detect.bat"
    } else {
        "bin/foobar-detect"
    });

    // Removing the entry point would break a reinstall-every-time
    // implementation, and is invisible to one that reuses the prefix.
    fs_err::remove_file(&entry_point).unwrap();
    let second = ensure_plugin_environment(options()).await.unwrap();

    assert_eq!(
        first, second,
        "the same channel must yield the same identity"
    );
    assert!(
        !entry_point.exists(),
        "the prefix was reinstalled instead of reused"
    );
}

/// The whole composition: install, run, validate, cache. A second call must be
/// served from the cache, and the answer must be the same either way.
#[tokio::test]
async fn detection_is_cached_between_calls() {
    use rattler_cache::virtual_package_plugin_cache::VirtualPackagePluginCache;
    use rattler_virtual_package_plugins::{DetectOptions, detect_virtual_packages};

    let cache = tempfile::tempdir().unwrap();
    let channel = fixture_channel();
    let plugin = PackageName::new_unchecked("foobar-detect");
    let platform = Platform::current();

    let gateway = Gateway::builder()
        .with_cache_dir(cache.path().join("repodata"))
        .with_package_cache(PackageCache::new(cache.path().join("pkgs")))
        .finish();
    let package_cache = PackageCache::new(cache.path().join("pkgs"));
    let detection_cache = VirtualPackagePluginCache::new(cache.path().join("detections"));
    let environment_root = cache.path().join("plugins");

    let declared: std::collections::BTreeSet<_> = gateway
        .virtual_package_plugins(&channel, platform)
        .await
        .unwrap()
        .get(&plugin)
        .expect("registered by the fixture channel")
        .iter()
        .cloned()
        .collect();

    let options = |now| DetectOptions {
        gateway: &gateway,
        package_cache: &package_cache,
        detection_cache: &detection_cache,
        channel: &channel,
        plugin: &plugin,
        declared: &declared,
        environment_root: &environment_root,
        host_platform: platform,
        now,
    };

    let first = detect_virtual_packages(options(1_000)).await.unwrap();
    assert!(!first.from_cache, "the first call has to run the plugin");

    let mut reported: Vec<String> = first
        .virtual_packages
        .iter()
        .map(|detected| detected.package.to_string())
        .collect();
    reported.sort();
    assert_eq!(reported, ["__foobar=1.2.3", "__foobar_arch=0=gen4"]);

    // Provenance travels with each virtual package.
    for detected in &first.virtual_packages {
        assert_eq!(detected.channel, channel.base_url);
    }

    let second = detect_virtual_packages(options(1_000)).await.unwrap();
    assert!(
        second.from_cache,
        "the second call must not rerun the plugin"
    );
    assert_eq!(
        second.virtual_packages, first.virtual_packages,
        "a cached answer must match the one that was cached"
    );

    // The fixture declares a one hour TTL, so past it the plugin runs again.
    let expired = detect_virtual_packages(options(1_000 + 3_600))
        .await
        .unwrap();
    assert!(!expired.from_cache, "an expired entry must not be reused");
    assert_eq!(expired.virtual_packages, first.virtual_packages);
}
