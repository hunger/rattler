//! Deciding which plugin speaks for a virtual package, within a view.
//!
//! A **view** is one channel together with every channel it reaches through a
//! CEP-42 `base` chain. It is the scope a virtual package lives in: a plugin's
//! verdict is visible to the channel that registered it and to anything deriving
//! from that channel, and nowhere else.
//!
//! Two consequences follow, and both are the point.
//!
//! **Unrelated channels never compete.** Two channels with no relation between
//! them may each register a plugin for `__rocm`, and both answer -- each within
//! its own view. A channel outside the chain cannot say anything about the
//! packages this one serves, so it contributes nothing, and nothing here
//! consults the order the channels were listed in.
//!
//! **Inside a view, CEP-42's priority decides.** The chain is built in the
//! order that CEP defines -- bases ahead of the channel declaring them,
//! overridden channels behind it -- and the first claimant along it wins. A
//! channel wanting to redefine a virtual package its upstream speaks for
//! declares `overrides`, which is the relation that means "I outrank that
//! channel"; `base` means the opposite.
//!
//! Two plugins in *one* channel claiming the same virtual package is neither. It
//! is a channel contradicting itself, with nothing to break the tie, so it is an
//! error.
//!
//! Built-ins are the weakest source of all: a plugin claiming a name the client
//! also detects overrides it. CEP 30 requires such a name to be *present* and
//! does not dictate that the client's own detection is what fills it.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rattler_conda_types::{Channel, ChannelUrl, PackageName, Platform};
use rattler_repodata_gateway::{
    DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH, Gateway, GatewayError, SubdirVirtualPackagePlugins,
    resolve_channel_relation,
};

/// What one channel registered, once its subdirs have been folded together.
type ChannelPlugins = IndexMap<PackageName, BTreeSet<PackageName>>;

/// One channel and every channel related to it, in priority order.
///
/// The scope a virtual package is resolved in. A channel outside this chain
/// cannot answer anything about the packages this one serves, so it contributes
/// nothing here; views over unrelated channels do not interact at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelView {
    /// The channel this view belongs to.
    pub channel: ChannelUrl,

    /// Every channel in scope, **highest priority first**, as CEP-42 orders
    /// them. A claim earlier in the chain wins over the same claim later in it.
    pub chain: Vec<ChannelUrl>,
}

/// The channels in scope for `channel`, in CEP-42 priority order.
///
/// Both relations are followed, and their directions are the CEP's:
///
/// - **`base`** names a channel the declaring one builds upon, which has
///   *higher* priority. Bases are walked transitively and end up ahead of the
///   declaring channel.
/// - **`overrides`** names a channel the declaring one supersedes, which has
///   *lower* priority. Overridden channels are walked transitively and end up
///   behind it.
///
/// So a channel declaring `base: conda-forge` and `overrides: my-hotfixes`
/// yields `[conda-forge, itself, my-hotfixes]`, which is the order CEP-42 spells
/// out. A channel that wants to redefine a virtual package its upstream already
/// speaks for declares `overrides`, not `base`: `base` means the upstream wins.
///
/// References resolve through [`resolve_channel_relation`], so a reference the
/// gateway would refuse is skipped here too, and one already in the chain ends
/// that walk, which terminates a cycle. The depth cap is CEP-42's own
/// [`DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH`].
pub async fn channel_view(
    gateway: &Gateway,
    channel: &Channel,
    platform: Platform,
) -> Result<ChannelView, GatewayError> {
    let mut seen = BTreeSet::from([channel.base_url.clone()]);
    let higher = walk(gateway, channel, platform, &mut seen, Relation::Base).await?;
    let lower = walk(gateway, channel, platform, &mut seen, Relation::Overrides).await?;

    // Bases are higher priority than the channel that declares them, so they
    // come first, nearest last; overridden channels are lower, so they follow.
    let chain = higher
        .into_iter()
        .rev()
        .chain([channel.base_url.clone()])
        .chain(lower)
        .collect();

    Ok(ChannelView {
        channel: channel.base_url.clone(),
        chain,
    })
}

/// Which CEP-42 edge to follow.
#[derive(Clone, Copy)]
enum Relation {
    /// Higher priority than the channel declaring it.
    Base,
    /// Lower priority than the channel declaring it.
    Overrides,
}

/// Follows one relation transitively, nearest first.
async fn walk(
    gateway: &Gateway,
    from: &Channel,
    platform: Platform,
    seen: &mut BTreeSet<ChannelUrl>,
    relation: Relation,
) -> Result<Vec<ChannelUrl>, GatewayError> {
    let mut found: Vec<ChannelUrl> = Vec::new();

    while found.len() <= DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH {
        let declaring = found
            .last()
            .cloned()
            .unwrap_or_else(|| from.base_url.clone());
        let Some(relations) = gateway
            .channel_relations(&Channel::from_url(declaring.clone()), platform)
            .await?
        else {
            break;
        };
        let reference = match relation {
            Relation::Base => relations.base,
            Relation::Overrides => relations.overrides,
        };
        let Some(next) = reference
            .as_deref()
            .and_then(|reference| resolve_channel_relation(&declaring, reference))
        else {
            break;
        };
        if !seen.insert(next.clone()) {
            break;
        }
        found.push(next);
    }

    Ok(found)
}

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

/// What one view resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedView {
    /// The channel whose view this is.
    pub channel: ChannelUrl,

    /// The plugins to run, highest-priority channel first and, within a
    /// channel, in the order the channel listed them. Each provides at least one
    /// virtual package.
    pub plugins: Vec<ResolvedPlugin>,

    /// Registrations that are not run at all, because a channel nearer the head
    /// of this view's chain already speaks for every name they claimed. Their
    /// `provides` is empty.
    ///
    /// Returned rather than dropped so a caller can say a registration was
    /// skipped and which channel took it, instead of leaving a user to wonder
    /// where their plugin went.
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

/// Works out which plugins to run in each view.
///
/// One [`ResolvedView`] per view, in the order the views were given. Views are
/// independent: a name claimed in one has no bearing on the same name in
/// another, so nothing here compares two channels that do not share a chain.
///
/// `registrations` is every registration a query saw, in any order; each view
/// takes the ones belonging to channels on its own chain. Subdirs of one channel
/// are folded together, since a channel repeats its registration in every subdir
/// and the same plugin appearing several times is one plugin registered for the
/// union of what those subdirs said.
pub fn resolve_views(
    views: &[ChannelView],
    registrations: impl IntoIterator<Item = SubdirVirtualPackagePlugins>,
) -> Result<Vec<ResolvedView>, Box<ConflictingClaim>> {
    let folded = fold_subdirs(registrations);
    views
        .iter()
        .map(|view| resolve_view(view, &folded))
        .collect()
}

/// Resolves one view, walking its chain most-derived first so that a channel
/// overrides the channels it derives from.
fn resolve_view(
    view: &ChannelView,
    folded: &IndexMap<ChannelUrl, ChannelPlugins>,
) -> Result<ResolvedView, Box<ConflictingClaim>> {
    let mut claimed: BTreeMap<PackageName, ChannelUrl> = BTreeMap::new();
    let mut resolved = ResolvedView {
        channel: view.channel.clone(),
        plugins: Vec::new(),
        shadowed: Vec::new(),
    };

    for channel in &view.chain {
        let Some(plugins) = folded.get(channel) else {
            continue;
        };
        check_for_self_conflict(channel, plugins)?;

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

            let plugin = ResolvedPlugin {
                channel: channel.clone(),
                plugin: plugin.clone(),
                declared: declared.clone(),
                provides,
                shadowed_by,
            };
            if plugin.provides.is_empty() {
                resolved.shadowed.push(plugin);
            } else {
                resolved.plugins.push(plugin);
            }
        }
    }

    Ok(resolved)
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

    /// A view over `chain`, most derived first.
    fn view(chain: &[&str]) -> ChannelView {
        ChannelView {
            channel: channel(chain[0]),
            chain: chain.iter().map(|name| channel(name)).collect(),
        }
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
        let resolved = resolve_views(
            &[view(&["org"])],
            [subdir(
                "org",
                Platform::Linux64,
                &[("rocm-detect", &["__rocm"])],
            )],
        )
        .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].plugins.len(), 1);
        assert_eq!(provides(&resolved[0].plugins[0]), ["__rocm"]);
        assert!(resolved[0].plugins[0].shadowed_by.is_empty());
        assert!(resolved[0].shadowed.is_empty());
    }

    /// CEP-42: a `base` is *higher* priority than the channel declaring it, so
    /// the base wins a contested name. `channel_view` puts it ahead in the
    /// chain, and first-in-chain wins.
    #[test]
    fn a_base_channel_outranks_the_channel_that_declares_it() {
        let resolved = resolve_views(
            // As channel_view builds it: base first, then the declaring channel.
            &[view(&["base", "derived"])],
            [
                subdir("base", Platform::Linux64, &[("a-detect", &["__cuda"])]),
                subdir("derived", Platform::Linux64, &[("b-detect", &["__cuda"])]),
            ],
        )
        .unwrap();

        assert_eq!(resolved[0].plugins.len(), 1, "the loser must not run");
        assert_eq!(resolved[0].plugins[0].channel, channel("base"));

        assert_eq!(resolved[0].shadowed.len(), 1);
        assert_eq!(resolved[0].shadowed[0].channel, channel("derived"));
        assert_eq!(
            resolved[0].shadowed[0].shadowed_by.get(&name("__cuda")),
            Some(&channel("base")),
            "a shadowed registration has to say who took it"
        );
    }

    /// CEP-42: `overrides` points at a channel of *lower* priority, so the
    /// declaring channel wins. This is the relation a channel uses to redefine
    /// a virtual package its upstream already speaks for.
    #[test]
    fn a_channel_outranks_what_it_overrides() {
        let resolved = resolve_views(
            // As channel_view builds it: the declaring channel, then what it
            // overrides.
            &[view(&["mine", "upstream"])],
            [
                subdir("upstream", Platform::Linux64, &[("a-detect", &["__cuda"])]),
                subdir("mine", Platform::Linux64, &[("b-detect", &["__cuda"])]),
            ],
        )
        .unwrap();

        assert_eq!(resolved[0].plugins.len(), 1);
        assert_eq!(resolved[0].plugins[0].channel, channel("mine"));
        assert_eq!(resolved[0].shadowed[0].channel, channel("upstream"));
    }

    /// The rule this replaces: two channels with no relationship used to fight
    /// over a name, and the one listed first won. They are separate views now
    /// and never meet, so both answer.
    #[test]
    fn independent_channels_do_not_compete() {
        let resolved = resolve_views(
            &[view(&["first"]), view(&["second"])],
            [
                subdir("first", Platform::Linux64, &[("a-detect", &["__rocm"])]),
                subdir("second", Platform::Linux64, &[("b-detect", &["__rocm"])]),
            ],
        )
        .unwrap();

        assert_eq!(resolved.len(), 2);
        for (index, expected) in [(0, "first"), (1, "second")] {
            assert_eq!(
                resolved[index].plugins.len(),
                1,
                "{expected} answers for __rocm in its own view"
            );
            assert_eq!(resolved[index].plugins[0].channel, channel(expected));
            assert!(
                resolved[index].shadowed.is_empty(),
                "there is no contest to lose"
            );
        }
    }

    /// A view only sees the channels on its own chain, so a registration
    /// elsewhere is not resolved into it at all.
    #[test]
    fn a_view_ignores_channels_outside_its_chain() {
        let resolved = resolve_views(
            &[view(&["org"])],
            [
                subdir("org", Platform::Linux64, &[("a-detect", &["__rocm"])]),
                subdir(
                    "elsewhere",
                    Platform::Linux64,
                    &[("b-detect", &["__oneapi"])],
                ),
            ],
        )
        .unwrap();

        assert_eq!(resolved[0].plugins.len(), 1);
        assert_eq!(provides(&resolved[0].plugins[0]), ["__rocm"]);
    }

    /// A plugin can lose one name to a higher-priority channel in its view and
    /// keep another. It still runs, and is still held to everything its channel
    /// registered it for.
    #[test]
    fn a_partially_shadowed_plugin_still_runs_for_what_it_wins() {
        let resolved = resolve_views(
            &[view(&["base", "derived"])],
            [
                subdir("base", Platform::Linux64, &[("a-detect", &["__rocm"])]),
                subdir(
                    "derived",
                    Platform::Linux64,
                    &[("b-detect", &["__rocm", "__oneapi"])],
                ),
            ],
        )
        .unwrap();

        assert_eq!(resolved[0].plugins.len(), 2);
        assert!(resolved[0].shadowed.is_empty(), "it still runs");

        let partial = &resolved[0].plugins[1];
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
        let resolved = resolve_views(
            &[view(&["org"])],
            [
                subdir("org", Platform::Linux64, &[("d-detect", &["__rocm"])]),
                subdir("org", Platform::NoArch, &[("d-detect", &["__oneapi"])]),
            ],
        )
        .unwrap();

        assert_eq!(resolved[0].plugins.len(), 1, "one plugin, not two");
        assert_eq!(
            provides(&resolved[0].plugins[0]),
            ["__oneapi", "__rocm"],
            "what the subdirs registered is unioned"
        );
    }

    /// Nothing can break this tie, and running either would be a guess.
    #[test]
    fn two_plugins_in_one_channel_claiming_one_name_is_an_error() {
        let error = resolve_views(
            &[view(&["org"])],
            [subdir(
                "org",
                Platform::Linux64,
                &[("a-detect", &["__rocm"]), ("b-detect", &["__rocm"])],
            )],
        )
        .unwrap_err();

        assert_eq!(error.virtual_package, name("__rocm"));
        assert_eq!(error.first, name("a-detect"));
        assert_eq!(error.second, name("b-detect"));
    }

    #[test]
    fn registering_nothing_resolves_to_nothing() {
        assert!(resolve_views(&[], []).unwrap().is_empty());

        let resolved =
            resolve_views(&[view(&["org"])], [subdir("org", Platform::Linux64, &[])]).unwrap();
        assert!(resolved[0].plugins.is_empty());
        assert!(resolved[0].shadowed.is_empty());
    }
}
