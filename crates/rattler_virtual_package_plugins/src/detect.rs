//! Detecting virtual packages with a channel's plugin, end to end.
//!
//! This is the composition of everything else in the crate: install the plugin,
//! run it, read what it said, hold it to what its channel promised, and keep the
//! answer so the next solve does not pay for it again.

use std::{collections::BTreeSet, path::Path};

use rattler_cache::{
    package_cache::PackageCache,
    virtual_package_plugin_cache::{CacheKey, CachedDetection, VirtualPackagePluginCache},
};
use rattler_conda_types::{Channel, ChannelVirtualPackage, PackageName, Platform};
use rattler_repodata_gateway::Gateway;

use crate::{
    contract::{self, ContractViolation},
    environment::{EnvironmentError, PluginEnvironmentOptions, ensure_plugin_environment},
    protocol::{ProtocolError, parse_output},
    runner::{RunnerError, run_plugin},
};

/// What a detection produced, and whether it had to run the plugin to get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The virtual packages the plugin reported present, each carrying the
    /// channel and the plugin environment that produced it.
    pub virtual_packages: Vec<ChannelVirtualPackage>,

    /// Whether this came from the cache rather than from running the plugin.
    pub from_cache: bool,
}

/// Detection failed. Every variant means the same thing for a solve -- none of
/// this plugin's virtual packages can be used -- but they are distinguished so a
/// caller can tell a broken plugin from a broken channel and say so.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    /// The plugin's environment could not be prepared.
    #[error(transparent)]
    Environment(#[from] EnvironmentError),

    /// The plugin could not be run.
    #[error(transparent)]
    Runner(#[from] RunnerError),

    /// The plugin ran and failed.
    ///
    /// Not an internal error: the plugin was reachable and said no. Callers
    /// generally treat this as "none of these virtual packages are present" and
    /// warn, since a system without the hardware is indistinguishable from here.
    #[error("the plugin exited with {}", match exit_code {
        Some(code) => code.to_string(),
        None => "a signal".to_string(),
    })]
    PluginFailed {
        /// The exit code, or `None` if a signal killed it.
        exit_code: Option<i32>,
        /// What the plugin wrote to stderr, for diagnosis.
        stderr: String,
    },

    /// The plugin's output was not valid protocol.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// The plugin reported something other than what its channel registered it
    /// for.
    #[error(transparent)]
    Contract(#[from] ContractViolation),

    /// The result could not be cached.
    #[error(transparent)]
    Cache(#[from] rattler_cache::virtual_package_plugin_cache::CacheError),
}

/// Everything needed to detect one plugin's virtual packages.
pub struct DetectOptions<'a> {
    /// Where to read the channel's repodata from.
    pub gateway: &'a Gateway,

    /// The package cache the plugin's install draws from.
    pub package_cache: &'a PackageCache,

    /// Where detection results are kept between runs.
    pub detection_cache: &'a VirtualPackagePluginCache,

    /// The channel that registered the plugin.
    pub channel: &'a Channel,

    /// The package providing the plugin.
    pub plugin: &'a PackageName,

    /// The virtual packages the channel registered this plugin for. The plugin
    /// is held to exactly this set.
    pub declared: &'a BTreeSet<PackageName>,

    /// Directory the per-plugin prefixes live under.
    pub environment_root: &'a Path,

    /// The platform to solve the plugin for; detection is host-only.
    pub host_platform: Platform,

    /// The current time in seconds since the Unix epoch, used for cache expiry.
    /// A parameter rather than read from the clock so callers and tests agree on
    /// what "now" is across a whole solve.
    pub now: i64,
}

/// Detects the virtual packages `plugin` speaks for, running it only if there is
/// no usable cached answer.
///
/// The plugin environment is prepared first even on a cache hit: the cache is
/// keyed by the contents of that environment, so there is nothing to look up
/// until it is known. What a hit saves is the install and the plugin run.
pub async fn detect_virtual_packages(options: DetectOptions<'_>) -> Result<Detection, DetectError> {
    let DetectOptions {
        gateway,
        package_cache,
        detection_cache,
        channel,
        plugin,
        declared,
        environment_root,
        host_platform,
        now,
    } = options;

    let environment = ensure_plugin_environment(PluginEnvironmentOptions {
        gateway,
        package_cache,
        channel,
        plugin,
        root: environment_root,
        host_platform,
    })
    .await?;

    let key = CacheKey::new(channel.base_url.clone(), plugin.clone(), environment.sha256);
    if let Some(cached) = detection_cache.get(&key, now)? {
        tracing::debug!("reusing cached verdicts for {}", plugin.as_source());
        return Ok(Detection {
            virtual_packages: cached.virtual_packages,
            from_cache: true,
        });
    }

    let run = run_plugin(
        &environment.prefix,
        plugin.as_source(),
        host_platform,
        declared.len(),
    )
    .await?;
    if !run.succeeded() {
        return Err(DetectError::PluginFailed {
            exit_code: run.exit_code,
            stderr: run.stderr,
        });
    }
    if !run.stderr.trim().is_empty() {
        tracing::debug!(
            "{} wrote to stderr: {}",
            plugin.as_source(),
            run.stderr.trim()
        );
    }

    let output = parse_output(&run.stdout)?;
    contract::validate(declared, &output)?;

    let virtual_packages: Vec<_> = output
        .detections
        .iter()
        .filter_map(|verdict| {
            Some(ChannelVirtualPackage {
                channel: channel.base_url.clone(),
                plugin_sha256: environment.sha256,
                package: verdict.to_generic()?,
            })
        })
        .collect();

    // Cached even when everything came back absent: "no such hardware here" is a
    // real answer and just as expensive to compute again.
    let policy = output.cache_policy.unwrap_or_default();
    let cached = CachedDetection::record(
        virtual_packages,
        policy.ttl_seconds,
        policy.watch_paths.iter().map(Into::into),
        now,
    );
    detection_cache.put(&key, &cached)?;

    Ok(Detection {
        virtual_packages: cached.virtual_packages,
        from_cache: false,
    })
}
