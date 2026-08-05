//! Every virtual package a solve over a set of channels should see.
//!
//! The pieces to do this have existed separately -- views, resolution,
//! factories, overrides -- and every caller that wanted the whole answer had to
//! assemble them in the right order: build a view per channel, collect the
//! registrations each view inherits, resolve who speaks for what, and only then
//! detect. Getting that order wrong is silent rather than loud, so it is done
//! once here.
//!
//! What comes back is the built-ins together with the plugin verdicts of every
//! channel in scope, which is what a solver wants. It is *not* per view: a solve
//! draws from all its channels at once, and CEP-42 priority has already decided
//! which channel's plugin speaks for a contested name by the time the values get
//! here.

use std::{collections::BTreeSet, path::Path};

use rattler_conda_types::{Channel, PackageName, Platform, SourcedVirtualPackage};
use rattler_repodata_gateway::{Gateway, GatewayError, SubdirVirtualPackagePlugins};

use crate::{
    factory::{
        BuiltinVirtualPackages, FactoryError, PluginContext, PluginVirtualPackages, resolve_needed,
    },
    overrides::PluginOverrides,
    resolve::{ViewError, resolve_views},
    runner::RunTimeout,
};

/// What assembling the virtual packages for a solve needs.
pub struct AssembleOptions<'a> {
    /// Where to read channel repodata from.
    pub gateway: &'a Gateway,

    /// The channels the solve draws from, as the user gave them. Channels these
    /// reach through CEP-42 relations are discovered rather than listed.
    pub channels: &'a [Channel],

    /// The platform being solved for. Detection is host-only, so this is also
    /// the platform plugins are solved for.
    pub platform: Platform,

    /// The package cache a plugin's install draws from.
    pub package_cache: &'a rattler_cache::package_cache::PackageCache,

    /// Where detection results are kept between runs.
    pub detection_cache: &'a rattler_cache::virtual_package_plugin_cache::VirtualPackagePluginCache,

    /// Directory the per-plugin prefixes live under.
    pub environment_root: &'a Path,

    /// How long a plugin may run before it is killed.
    pub timeout: RunTimeout,

    /// The current time in seconds since the Unix epoch, for cache expiry.
    pub now: i64,

    /// What the environment says a virtual package is, standing in for detecting
    /// it.
    pub overrides: &'a PluginOverrides,

    /// The virtual package names the solve could ask for, from
    /// [`virtual_packages_mentioned`](crate::demand::virtual_packages_mentioned).
    /// A plugin speaking only for names outside this set is never run.
    pub needed: &'a BTreeSet<PackageName>,
}

/// Assembling the virtual packages for a solve failed.
#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    /// A channel's repodata could not be read.
    #[error(transparent)]
    Gateway(#[from] GatewayError),

    /// A channel's CEP-42 relations are something a client must refuse.
    #[error(transparent)]
    View(#[from] Box<ViewError>),

    /// Two channels in one view claim the same virtual package, or a plugin
    /// could not be run.
    #[error(transparent)]
    Factory(#[from] Box<FactoryError>),

    /// A view could not be resolved.
    #[error("the channels' virtual package plugins could not be resolved: {0}")]
    Resolve(String),
}

/// The virtual packages a solve over `options.channels` should be given.
///
/// The built-ins are always included, since CEP 30 obliges a client to offer
/// them. A channel's plugin is run only if the solve mentions one of the names
/// it won, and not at all if the environment already answers for all of them.
pub async fn virtual_packages_for_solve(
    options: AssembleOptions<'_>,
) -> Result<Vec<SourcedVirtualPackage>, AssembleError> {
    let views = view_per_channel(options.gateway, options.channels, options.platform).await?;
    let in_scope = channels_in_scope(&views);

    let mut registrations = Vec::new();
    for channel in &in_scope {
        let plugins = options
            .gateway
            .virtual_package_plugins(channel, options.platform)
            .await?;
        if !plugins.is_empty() {
            registrations.push(SubdirVirtualPackagePlugins {
                channel: channel.base_url.clone(),
                platform: options.platform,
                plugins,
            });
        }
    }

    let resolved_views = resolve_views(&views, registrations)
        .map_err(|err| AssembleError::Resolve(err.to_string()))?;

    let context = PluginContext {
        gateway: options.gateway,
        package_cache: options.package_cache,
        detection_cache: options.detection_cache,
        environment_root: options.environment_root,
        host_platform: options.platform,
        timeout: options.timeout,
        now: options.now,
        overrides: options.overrides,
    };

    // One factory per plugin that won something, across every view. A plugin
    // that lost all its names is not here at all: it has nothing to say that
    // another channel is not already saying.
    let factories: Vec<_> = resolved_views
        .iter()
        .flat_map(|view| &view.plugins)
        .filter(|resolved| !resolved.provides.is_empty())
        .filter_map(|resolved| {
            let channel = in_scope
                .iter()
                .find(|channel| channel.base_url == resolved.channel)?;
            Some(PluginVirtualPackages::new(resolved, channel, context))
        })
        .collect();

    resolve_needed(
        &BuiltinVirtualPackages::from_env(),
        &factories,
        options.needed,
    )
    .await
    .map_err(Box::new)
    .map_err(AssembleError::Factory)
}

/// A view per channel the caller named.
async fn view_per_channel(
    gateway: &Gateway,
    channels: &[Channel],
    platform: Platform,
) -> Result<Vec<crate::resolve::ChannelView>, AssembleError> {
    let mut views = Vec::new();
    for channel in channels {
        views.push(
            crate::resolve::channel_view(gateway, channel, platform)
                .await
                .map_err(Box::new)?,
        );
    }
    Ok(views)
}

/// Every channel any view can see, deduplicated.
///
/// Not just the ones named on the command line: a view inherits the
/// registrations of the channels it reaches, and a base is usually not something
/// the user listed.
fn channels_in_scope(views: &[crate::resolve::ChannelView]) -> Vec<Channel> {
    let mut in_scope: Vec<Channel> = Vec::new();
    for url in views.iter().flat_map(|view| &view.chain) {
        if !in_scope.iter().any(|channel| channel.base_url == *url) {
            in_scope.push(Channel::from_url(url.clone()));
        }
    }
    in_scope
}
