//! Deciding which plugin speaks for a virtual package.
//!
//! Two channels may each register a plugin for `__rocm`, and nothing in the
//! metadata prevents it. The gateway reports both claims verbatim -- deciding
//! between them is this module's job, and the rule is the one channels already
//! follow everywhere else: **the highest-priority channel wins**.
//!
//! Two plugins in *one* channel claiming the same virtual package is a
//! different thing entirely. There is no priority to break that tie, and the
//! channel is contradicting itself, so it is an error rather than a choice.
//!
//! What this does not decide is whether a plugin may speak for a virtual
//! package clients detect themselves, such as `__cuda` or `__glibc`. That
//! policy is still open; shadowing is reported elsewhere and nothing acts on it.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rattler_conda_types::{ChannelUrl, PackageName};
use rattler_repodata_gateway::SubdirVirtualPackagePlugins;

/// What one channel registered, once its subdirs have been folded together.
type ChannelPlugins = IndexMap<PackageName, BTreeSet<PackageName>>;

/// What a channel registered, and how much of it survived the contest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlugin {
    /// The channel that registered it.
    pub channel: ChannelUrl,

    /// The package providing it.
    pub plugin: PackageName,

    /// Everything its channel registered it for, across every subdir.
    ///
    /// The plugin is still held to all of it: the contract is between the
    /// plugin and its channel, and losing a name to a higher-priority channel
    /// does not excuse the plugin from giving a verdict on it.
    pub declared: BTreeSet<PackageName>,

    /// The subset of [`declared`](Self::declared) whose verdicts are used.
    pub provides: BTreeSet<PackageName>,

    /// For each name this plugin lost, the channel that speaks for it instead.
    /// Empty when the plugin won everything it claimed.
    pub shadowed_by: BTreeMap<PackageName, ChannelUrl>,
}

/// Which plugins to run, and which registrations lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// The plugins to run, in channel-priority order and, within a channel, in
    /// the order the channel listed them. Each provides at least one virtual
    /// package.
    pub plugins: Vec<ResolvedPlugin>,

    /// Registrations that are not run at all, because a higher-priority channel
    /// already speaks for every name they claimed. Their `provides` is empty.
    ///
    /// Returned rather than dropped so a caller can say a registration was
    /// skipped and why, instead of leaving a user to wonder where their plugin
    /// went.
    pub shadowed: Vec<ResolvedPlugin>,
}

/// A channel registered two different plugins for one virtual package.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "the channel '{channel}' registers both '{}' and '{}' for '{}', and nothing says which of \
     them speaks for it",
    first.as_source(),
    second.as_source(),
    virtual_package.as_source()
)]
pub struct ConflictingClaim {
    /// The channel that contradicts itself.
    pub channel: ChannelUrl,
    /// The virtual package claimed twice.
    pub virtual_package: PackageName,
    /// The plugin that claimed it first, in the channel's own order.
    pub first: PackageName,
    /// The plugin that claimed it again.
    pub second: PackageName,
}

/// Works out which plugins to run, given every registration a query saw.
///
/// `registrations` must be in resolved channel-priority order, which is what
/// `RepoDataQueryOutput::virtual_package_plugins` yields. Subdirs of one channel
/// are folded together: a channel repeats its registration in every subdir, so
/// the same plugin appearing several times is one plugin, and what it is
/// registered for is the union.
///
/// Plugins come back in channel-priority order, and within a channel in the
/// order the channel listed them.
pub fn resolve_plugins(
    registrations: impl IntoIterator<Item = SubdirVirtualPackagePlugins>,
) -> Result<Resolution, Box<ConflictingClaim>> {
    let mut claimed: BTreeMap<PackageName, ChannelUrl> = BTreeMap::new();
    let mut resolution = Resolution::default();

    for (channel, plugins) in fold_subdirs(registrations) {
        check_for_self_conflict(&channel, &plugins)?;

        for (plugin, declared) in plugins {
            let shadowed_by: BTreeMap<_, _> = declared
                .iter()
                .filter_map(|name| Some((name.clone(), claimed.get(name)?.clone())))
                .collect();
            let provides: BTreeSet<_> = declared
                .iter()
                .filter(|name| !shadowed_by.contains_key(*name))
                .cloned()
                .collect();

            for name in &provides {
                claimed.insert(name.clone(), channel.clone());
            }

            let resolved = ResolvedPlugin {
                channel: channel.clone(),
                plugin,
                declared,
                provides,
                shadowed_by,
            };
            if resolved.provides.is_empty() {
                resolution.shadowed.push(resolved);
            } else {
                resolution.plugins.push(resolved);
            }
        }
    }

    Ok(resolution)
}

/// Rejects a channel that registered two different plugins for one virtual
/// package, before any of its plugins is run.
fn check_for_self_conflict(
    channel: &ChannelUrl,
    plugins: &ChannelPlugins,
) -> Result<(), Box<ConflictingClaim>> {
    let mut claimed_here: BTreeMap<&PackageName, &PackageName> = BTreeMap::new();
    for (plugin, declared) in plugins {
        for virtual_package in declared {
            if let Some(first) = claimed_here.insert(virtual_package, plugin) {
                return Err(Box::new(ConflictingClaim {
                    channel: channel.clone(),
                    virtual_package: virtual_package.clone(),
                    first: first.clone(),
                    second: plugin.clone(),
                }));
            }
        }
    }
    Ok(())
}

/// One entry per channel, in the order the channels first appear, each mapping
/// a plugin to everything that channel's subdirs registered it for.
fn fold_subdirs(
    registrations: impl IntoIterator<Item = SubdirVirtualPackagePlugins>,
) -> IndexMap<ChannelUrl, ChannelPlugins> {
    let mut channels: IndexMap<ChannelUrl, ChannelPlugins> = IndexMap::new();

    for subdir in registrations {
        let plugins = channels.entry(subdir.channel).or_default();
        for (plugin, declared) in subdir.plugins {
            plugins.entry(plugin).or_default().extend(declared);
        }
    }

    channels
}

#[cfg(test)]
mod tests {
    use rattler_conda_types::Platform;

    use super::*;

    fn channel(name: &str) -> ChannelUrl {
        url::Url::parse(&format!("https://prefix.dev/{name}/"))
            .expect("a valid channel url")
            .into()
    }

    fn name(name: &str) -> PackageName {
        PackageName::new_unchecked(name)
    }

    /// One subdir's registration: `(plugin, [virtual packages])` pairs.
    fn subdir(
        channel_name: &str,
        platform: Platform,
        plugins: &[(&str, &[&str])],
    ) -> SubdirVirtualPackagePlugins {
        SubdirVirtualPackagePlugins {
            channel: channel(channel_name),
            platform,
            plugins: plugins
                .iter()
                .map(|(plugin, provides)| {
                    (name(plugin), provides.iter().map(|p| name(p)).collect())
                })
                .collect(),
        }
    }

    fn provides(resolved: &ResolvedPlugin) -> Vec<&str> {
        resolved
            .provides
            .iter()
            .map(PackageName::as_source)
            .collect()
    }

    #[test]
    fn a_single_uncontested_plugin_is_resolved() {
        let resolution = resolve_plugins([subdir(
            "org",
            Platform::Linux64,
            &[("rocm-detect", &["__rocm"])],
        )])
        .unwrap();

        assert_eq!(resolution.plugins.len(), 1);
        assert_eq!(provides(&resolution.plugins[0]), ["__rocm"]);
        assert!(resolution.plugins[0].shadowed_by.is_empty());
        assert!(resolution.shadowed.is_empty());
    }

    /// The rule the review settled on: between channels, priority decides.
    #[test]
    fn the_highest_priority_channel_wins_a_contested_name() {
        let resolution = resolve_plugins([
            subdir("first", Platform::Linux64, &[("a-detect", &["__rocm"])]),
            subdir("second", Platform::Linux64, &[("b-detect", &["__rocm"])]),
        ])
        .unwrap();

        assert_eq!(resolution.plugins.len(), 1, "the loser must not be run");
        assert_eq!(resolution.plugins[0].channel, channel("first"));
        assert_eq!(resolution.plugins[0].plugin.as_source(), "a-detect");

        // The loser is reported rather than dropped, so it can be explained.
        assert_eq!(resolution.shadowed.len(), 1);
        assert_eq!(resolution.shadowed[0].plugin.as_source(), "b-detect");
        assert_eq!(
            resolution.shadowed[0].shadowed_by.get(&name("__rocm")),
            Some(&channel("first")),
            "the loser has to say who took it"
        );
    }

    /// A plugin can lose one name and keep another. It still runs, and it is
    /// still held to everything its channel registered it for -- only the
    /// verdicts for the lost names go nowhere.
    #[test]
    fn a_partially_shadowed_plugin_still_runs_for_what_it_wins() {
        let resolution = resolve_plugins([
            subdir("first", Platform::Linux64, &[("a-detect", &["__rocm"])]),
            subdir(
                "second",
                Platform::Linux64,
                &[("b-detect", &["__rocm", "__oneapi"])],
            ),
        ])
        .unwrap();

        assert_eq!(resolution.plugins.len(), 2);
        assert!(resolution.shadowed.is_empty(), "it still runs");

        let partial = &resolution.plugins[1];
        assert_eq!(provides(partial), ["__oneapi"]);
        assert_eq!(
            partial.shadowed_by.keys().collect::<Vec<_>>(),
            [&name("__rocm")]
        );
        assert!(
            partial.declared.contains(&name("__rocm")),
            "the contract still covers the name it lost"
        );
    }

    /// A channel repeats its registration in every subdir, so the same plugin
    /// seen twice is one plugin -- not a channel contradicting itself.
    #[test]
    fn subdirs_of_one_channel_are_folded_rather_than_compared() {
        let resolution = resolve_plugins([
            subdir("org", Platform::Linux64, &[("d-detect", &["__rocm"])]),
            subdir("org", Platform::NoArch, &[("d-detect", &["__oneapi"])]),
        ])
        .unwrap();

        assert_eq!(resolution.plugins.len(), 1, "one plugin, not two");
        assert_eq!(
            provides(&resolution.plugins[0]),
            ["__oneapi", "__rocm"],
            "what the subdirs registered is unioned"
        );
    }

    /// Nothing can break this tie, and running either would be a guess.
    #[test]
    fn two_plugins_in_one_channel_claiming_one_name_is_an_error() {
        let error = resolve_plugins([subdir(
            "org",
            Platform::Linux64,
            &[("a-detect", &["__rocm"]), ("b-detect", &["__rocm"])],
        )])
        .unwrap_err();

        assert_eq!(error.virtual_package, name("__rocm"));
        assert_eq!(error.first, name("a-detect"));
        assert_eq!(error.second, name("b-detect"));
    }

    /// The conflict is within one channel, so the same two plugin names in two
    /// different channels is the ordinary contest, not the error.
    #[test]
    fn the_same_claim_from_two_channels_is_not_a_conflict() {
        let resolution = resolve_plugins([
            subdir("first", Platform::Linux64, &[("a-detect", &["__rocm"])]),
            subdir("second", Platform::Linux64, &[("a-detect", &["__rocm"])]),
        ])
        .unwrap();

        assert_eq!(resolution.plugins.len(), 1);
        assert_eq!(resolution.plugins[0].channel, channel("first"));
    }

    #[test]
    fn registering_nothing_resolves_to_nothing() {
        assert_eq!(resolve_plugins([]).unwrap(), Resolution::default());
        assert_eq!(
            resolve_plugins([subdir("org", Platform::Linux64, &[])]).unwrap(),
            Resolution::default()
        );
    }
}
