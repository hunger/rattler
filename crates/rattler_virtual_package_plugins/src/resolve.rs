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

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    sync::LazyLock,
};

use indexmap::IndexMap;
use rattler_conda_types::{Channel, ChannelRelations, ChannelUrl, PackageName, Platform};
use rattler_repodata_gateway::{
    DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH, Gateway, GatewayError, SubdirVirtualPackagePlugins,
    resolve_channel_relation,
};
use regex::Regex;

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

/// Building a view ran into something CEP-42 says a client must refuse.
#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    /// The channel's repodata could not be read.
    #[error(transparent)]
    Gateway(#[from] GatewayError),

    /// CEP-42: "A channel MUST NOT declare both `base` and `overrides`
    /// referencing the same channel; clients MUST treat this as an error."
    ///
    /// The two say opposite things about priority, so a channel naming one
    /// channel in both has asked to be above and below it at once.
    #[error(
        "the channel '{channel}' declares '{reference}' as both its base and a channel it \
         overrides, which cannot both be true"
    )]
    ContradictoryRelations {
        /// The channel that declared both.
        channel: ChannelUrl,
        /// The channel named twice.
        reference: ChannelUrl,
    },

    /// CEP-42: "Clients MUST detect cycles in this graph and abort resolution
    /// with an error when a cycle is detected."
    #[error(
        "the channel relations reachable from '{channel}' form a cycle, so no priority order \
         exists for them"
    )]
    Cycle {
        /// The channel the view was being built for.
        channel: ChannelUrl,
    },

    /// CEP-42: "If the depth limit is exceeded, the client SHOULD abort
    /// resolution with an error."
    #[error(
        "following the channel relations of '{channel}' went deeper than {limit} channels, which \
         is as far as CEP-42 allows"
    )]
    TooDeep {
        /// The channel the view was being built for.
        channel: ChannelUrl,
        /// The cap that was exceeded.
        limit: usize,
    },
}

/// The channels in scope for `channel`, in CEP-42 priority order.
///
/// This is CEP-42's own algorithm, restricted to what one channel can reach.
/// Every channel reachable through a relation is discovered, each relation
/// contributes a priority edge, and the result is a topological sort of that
/// graph:
///
/// - **`base`** names a channel the declaring one builds upon, giving an edge
///   *from the base to the declaring channel*: the base has higher priority.
/// - **`overrides`** names a channel the declaring one supersedes, giving an
///   edge *from the declaring channel to the overridden one*: the declaring
///   channel has higher priority.
///
/// So a channel declaring `base: conda-forge` and `overrides: my-hotfixes`
/// yields `[conda-forge, itself, my-hotfixes]`. A channel that wants to redefine
/// a virtual package its upstream already speaks for declares `overrides`, not
/// `base`: `base` means the upstream wins.
///
/// A graph rather than two walks because relations compose in ways a walk
/// cannot see: a channel's base may itself override something, and that
/// something belongs in the order too. It is also what makes the cycle the CEP
/// requires clients to reject detectable at all.
///
/// References resolve through [`resolve_channel_relation`], so a reference the
/// gateway would refuse is ignored here too.
pub async fn channel_view(
    gateway: &Gateway,
    channel: &Channel,
    platform: Platform,
) -> Result<ChannelView, ViewError> {
    let root = channel.base_url.clone();
    let chain = relation_chain(root.clone(), |current| async move {
        gateway
            .channel_relations(&Channel::from_url(current), platform)
            .await
    })
    .await?;

    Ok(ChannelView {
        chain,
        channel: root,
    })
}

/// The priority order of `root` and every channel reachable from it, following
/// `relations_of` from one channel to the next.
///
/// Split from [`channel_view`] so the rules CEP-42 lays on the graph -- the
/// depth cap, the contradiction, and the cycle it requires clients to reject --
/// can be exercised without standing up a gateway that serves repodata declaring
/// each of them.
async fn relation_chain<Lookup, Fetch>(
    root: ChannelUrl,
    relations_of: Lookup,
) -> Result<Vec<ChannelUrl>, ViewError>
where
    Lookup: Fn(ChannelUrl) -> Fetch,
    Fetch: Future<Output = Result<Option<ChannelRelations>, GatewayError>>,
{
    let mut nodes: Vec<ChannelUrl> = vec![root.clone()];
    let mut edges: Vec<(ChannelUrl, ChannelUrl)> = Vec::new();
    let mut visited: BTreeSet<ChannelUrl> = BTreeSet::new();
    let mut queue: VecDeque<(ChannelUrl, usize)> = VecDeque::from([(root.clone(), 0)]);

    while let Some((current, depth)) = queue.pop_front() {
        if !visited.insert(current.clone()) {
            continue;
        }
        if depth > DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH {
            return Err(ViewError::TooDeep {
                channel: root,
                limit: DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH,
            });
        }

        let Some(relations) = relations_of(current.clone()).await? else {
            continue;
        };
        let resolve = |reference: Option<String>| {
            reference
                .as_deref()
                .and_then(|reference| resolve_channel_relation(&current, reference))
        };
        let base = resolve(relations.base);
        let overrides = resolve(relations.overrides);

        if let (Some(base), Some(overrides)) = (&base, &overrides)
            && base == overrides
        {
            return Err(ViewError::ContradictoryRelations {
                channel: current.clone(),
                reference: base.clone(),
            });
        }

        // A base outranks the channel naming it; a channel outranks what it
        // overrides. Both directions are the CEP's.
        for (higher, lower, next) in [
            base.map(|base| (base.clone(), current.clone(), base)),
            overrides.map(|overridden| (current.clone(), overridden.clone(), overridden)),
        ]
        .into_iter()
        .flatten()
        {
            edges.push((higher, lower));
            if !nodes.contains(&next) {
                nodes.push(next.clone());
            }
            queue.push_back((next, depth + 1));
        }
    }

    topological_order(&nodes, &edges).ok_or(ViewError::Cycle { channel: root })
}

/// Orders `nodes` so that every `(higher, lower)` edge puts `higher` first, or
/// `None` if the edges contain a cycle.
///
/// Kahn's algorithm, taking ready nodes in discovery order so that a graph with
/// several valid orders always produces the same one -- which channels get
/// visited is metadata-driven, and a solve that shuffles between runs would be
/// worse than one that is merely wrong.
fn topological_order(
    nodes: &[ChannelUrl],
    edges: &[(ChannelUrl, ChannelUrl)],
) -> Option<Vec<ChannelUrl>> {
    let mut incoming: BTreeMap<&ChannelUrl, usize> =
        nodes.iter().map(|node| (node, 0usize)).collect();
    for (_, lower) in edges {
        *incoming.entry(lower).or_default() += 1;
    }

    let mut ordered = Vec::with_capacity(nodes.len());
    let mut ready: VecDeque<&ChannelUrl> = nodes
        .iter()
        .filter(|node| incoming.get(node).copied().unwrap_or_default() == 0)
        .collect();

    while let Some(node) = ready.pop_front() {
        ordered.push(node.clone());
        for (higher, lower) in edges.iter().filter(|(higher, _)| higher == node) {
            let _ = higher;
            let count = incoming.entry(lower).or_default();
            *count = count.saturating_sub(1);
            if *count == 0 {
                ready.push_back(lower);
            }
        }
    }

    (ordered.len() == nodes.len()).then_some(ordered)
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

/// Whether `name` is a virtual package name CEP 26 allows.
///
/// The pattern is the CEP's own, quoted rather than reimplemented: getting a
/// character class subtly wrong here would either reject legitimate names or
/// admit ones the rest of the ecosystem will refuse.
///
/// Registrations reach this from channel metadata, so they are parsed leniently
/// -- one malformed entry must not make a whole `repodata.json` unusable -- but
/// parsing leniently is not the same as acting on the result. A name the CEP
/// forbids is dropped here rather than carried into a view, where it would fail
/// later as an unusable package spec.
fn is_valid_virtual_package_name(name: &PackageName) -> bool {
    /// CEP 26: "Virtual package names MUST follow this regex."
    static VALID: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^__[a-z0-9][._-]?([a-z0-9]+(\.|-|_|$))*$")
            .expect("the pattern is a literal from CEP 26")
    });
    /// CEP 26: "the maximum length of a package name MUST NOT exceed 64
    /// characters."
    const MAX_LENGTH: usize = 64;

    let name = name.as_normalized();
    name.len() <= MAX_LENGTH && VALID.is_match(name)
}

/// One entry per channel, in the order the channels first appear, each mapping
/// a plugin to everything that channel's subdirs registered it for.
fn fold_subdirs(
    registrations: impl IntoIterator<Item = SubdirVirtualPackagePlugins>,
) -> IndexMap<ChannelUrl, ChannelPlugins> {
    let mut channels: IndexMap<ChannelUrl, ChannelPlugins> = IndexMap::new();

    for subdir in registrations {
        let channel = subdir.channel.clone();
        let plugins = channels.entry(subdir.channel).or_default();
        for (plugin, declared) in subdir.plugins {
            let (valid, rejected): (Vec<_>, Vec<_>) = declared
                .into_iter()
                .partition(is_valid_virtual_package_name);
            for name in rejected {
                tracing::warn!(
                    "ignoring '{}', which '{}' registers '{}' for: CEP 26 does not allow it as a \
                     virtual package name",
                    name.as_source(),
                    channel,
                    plugin.as_source()
                );
            }
            plugins.entry(plugin).or_default().extend(valid);
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

    /// CEP-42 requires a client to reject a relation cycle rather than pick an
    /// arbitrary order, and a cycle is only visible once the relations are a
    /// graph: a walk that stops at a channel it has already seen cannot tell a
    /// cycle from a diamond.
    #[test]
    fn a_cycle_has_no_order() {
        let a = channel("a");
        let b = channel("b");
        assert_eq!(
            topological_order(&[a.clone(), b.clone()], &[(a.clone(), b.clone()), (b, a)]),
            None
        );
    }

    /// A diamond is not a cycle: two paths reaching the same channel still
    /// admit an order, and refusing it would reject a legitimate arrangement.
    #[test]
    fn a_diamond_still_has_an_order() {
        let (top, left, right, bottom) = (
            channel("top"),
            channel("left"),
            channel("right"),
            channel("bottom"),
        );
        let ordered = topological_order(
            &[top.clone(), left.clone(), right.clone(), bottom.clone()],
            &[
                (top.clone(), left.clone()),
                (top.clone(), right.clone()),
                (left.clone(), bottom.clone()),
                (right.clone(), bottom.clone()),
            ],
        )
        .expect("a diamond is orderable");

        let position = |c: &ChannelUrl| ordered.iter().position(|o| o == c).unwrap();
        assert!(position(&top) < position(&left));
        assert!(position(&top) < position(&right));
        assert!(position(&left) < position(&bottom));
        assert!(position(&right) < position(&bottom));
    }

    /// The order must not wander between runs: which channels are involved is
    /// metadata-driven, and a solve that shuffles would be worse than one that
    /// is merely wrong.
    #[test]
    fn the_order_is_stable() {
        let nodes = [channel("a"), channel("b"), channel("c")];
        let edges = [(channel("a"), channel("c"))];
        let first = topological_order(&nodes, &edges).unwrap();
        for _ in 0..8 {
            assert_eq!(topological_order(&nodes, &edges).unwrap(), first);
        }
    }

    /// Relations as a channel would declare them: `(channel, base, overrides)`,
    /// where the references are CEP-42 relative paths.
    fn declaring(
        relations: &[(&str, Option<&str>, Option<&str>)],
    ) -> BTreeMap<ChannelUrl, ChannelRelations> {
        relations
            .iter()
            .map(|(name, base, overrides)| {
                (
                    channel(name),
                    ChannelRelations {
                        base: base.map(ToString::to_string),
                        overrides: overrides.map(ToString::to_string),
                    },
                )
            })
            .collect()
    }

    /// Walks `relations` from `root`, standing in for a gateway serving repodata
    /// that declares them.
    async fn chain_from(
        root: &str,
        relations: &[(&str, Option<&str>, Option<&str>)],
    ) -> Result<Vec<ChannelUrl>, ViewError> {
        let declared = declaring(relations);
        relation_chain(channel(root), |current| {
            let relations = declared.get(&current).cloned();
            async move { Ok(relations) }
        })
        .await
    }

    /// The `base`/`overrides` directions, through the walk rather than through
    /// `topological_order` on hand-built edges.
    #[tokio::test]
    async fn a_chain_follows_both_kinds_of_relation() {
        let chain = chain_from("mine", &[("mine", Some("../up/"), Some("../old/"))])
            .await
            .expect("these relations are orderable");

        assert_eq!(chain, [channel("up"), channel("mine"), channel("old")]);
    }

    /// CEP-42: "Clients MUST detect cycles in this graph and abort resolution
    /// with an error when a cycle is detected."
    #[tokio::test]
    async fn a_cycle_between_channels_is_refused() {
        let error = chain_from(
            "a",
            &[("a", Some("../b/"), None), ("b", Some("../a/"), None)],
        )
        .await
        .expect_err("two channels each based on the other cannot be ordered");

        assert!(
            matches!(&error, ViewError::Cycle { channel: root } if root == &channel("a")),
            "expected a cycle rooted at the channel asked for, got: {error}"
        );
    }

    /// A cycle that closes further out is still a cycle: the walk has to detect
    /// it wherever it is, not only when the root is part of it.
    #[tokio::test]
    async fn a_cycle_beyond_the_root_is_refused() {
        let error = chain_from(
            "a",
            &[
                ("a", Some("../b/"), None),
                ("b", Some("../c/"), None),
                ("c", Some("../b/"), None),
            ],
        )
        .await
        .expect_err("a cycle among the channels reached is still a cycle");

        assert!(matches!(error, ViewError::Cycle { .. }), "got: {error}");
    }

    /// CEP-42: "A channel MUST NOT declare both `base` and `overrides`
    /// referencing the same channel; clients MUST treat this as an error."
    #[tokio::test]
    async fn naming_one_channel_as_both_base_and_overridden_is_refused() {
        let error = chain_from("mine", &[("mine", Some("../up/"), Some("../up/"))])
            .await
            .expect_err("one channel cannot be both above and below another");

        assert!(
            matches!(
                &error,
                ViewError::ContradictoryRelations { channel: declaring, reference }
                    if declaring == &channel("mine") && reference == &channel("up")
            ),
            "expected the declaring channel and the channel it named twice, got: {error}"
        );
    }

    /// Only *one channel* declaring both roles is the contradiction. Two
    /// channels naming the same base is an ordinary diamond, and refusing it
    /// would reject a legitimate arrangement.
    #[tokio::test]
    async fn two_channels_sharing_a_base_is_not_a_contradiction() {
        let chain = chain_from(
            "a",
            &[
                ("a", Some("../shared/"), Some("../b/")),
                ("b", Some("../shared/"), None),
            ],
        )
        .await
        .expect("two channels may share a base");

        assert_eq!(chain, [channel("shared"), channel("a"), channel("b")]);
    }

    /// CEP-42: "If the depth limit is exceeded, the client SHOULD abort
    /// resolution with an error." Each hop through a relation costs one.
    #[tokio::test]
    async fn relations_deeper_than_the_cap_are_refused() {
        let names: Vec<String> = (0..=DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH + 2)
            .map(|step| format!("step{step}"))
            .collect();
        let references: Vec<String> = names.iter().map(|name| format!("../{name}/")).collect();
        let relations: Vec<(&str, Option<&str>, Option<&str>)> = names
            .iter()
            .enumerate()
            .map(|(step, name)| {
                (
                    name.as_str(),
                    references.get(step + 1).map(String::as_str),
                    None,
                )
            })
            .collect();

        let error = chain_from(&names[0], &relations)
            .await
            .expect_err("a chain longer than the cap must not be followed");

        assert!(
            matches!(
                &error,
                ViewError::TooDeep { channel: root, limit }
                    if root == &channel("step0") && *limit == DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH
            ),
            "expected the root and the cap that was exceeded, got: {error}"
        );
    }

    /// A chain exactly at the cap is allowed: the limit is the last depth that
    /// still resolves, so an off-by-one here would reject valid metadata.
    #[tokio::test]
    async fn relations_up_to_the_cap_are_followed() {
        let names: Vec<String> = (0..=DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH)
            .map(|step| format!("step{step}"))
            .collect();
        let references: Vec<String> = names.iter().map(|name| format!("../{name}/")).collect();
        let relations: Vec<(&str, Option<&str>, Option<&str>)> = names
            .iter()
            .enumerate()
            .map(|(step, name)| {
                (
                    name.as_str(),
                    references.get(step + 1).map(String::as_str),
                    None,
                )
            })
            .collect();

        let chain = chain_from(&names[0], &relations)
            .await
            .expect("a chain at the cap is still within it");

        assert_eq!(chain.len(), names.len());
    }

    /// A reference the gateway would refuse -- anything not a `../` relative
    /// path -- contributes no edge rather than failing the whole view.
    #[tokio::test]
    async fn an_unresolvable_reference_is_ignored() {
        let chain = chain_from(
            "mine",
            &[("mine", Some("https://elsewhere.example/evil/"), None)],
        )
        .await
        .expect("a reference that does not resolve is skipped");

        assert_eq!(chain, [channel("mine")]);
    }

    /// CEP 26 says what a virtual package may be called. A registration naming
    /// something else is dropped rather than carried into a view, where it would
    /// fail later as an unusable package spec.
    #[test]
    fn a_name_cep_26_forbids_is_not_resolved() {
        let resolved = resolve_views(
            &[view(&["org"])],
            [subdir(
                "org",
                Platform::Linux64,
                &[("d-detect", &["__rocm", "no-underscores", "__UPPER", "__"])],
            )],
        )
        .unwrap();

        assert_eq!(
            provides(&resolved[0].plugins[0]),
            ["__rocm"],
            "only the legal name survives"
        );
    }

    /// A registration whose names are all illegal provides nothing, so the
    /// plugin is not run at all -- but the channel is otherwise unaffected.
    #[test]
    fn one_bad_name_does_not_spoil_a_channel() {
        let resolved = resolve_views(
            &[view(&["org"])],
            [subdir(
                "org",
                Platform::Linux64,
                &[("bad-detect", &["nonsense"]), ("good-detect", &["__rocm"])],
            )],
        )
        .unwrap();

        assert_eq!(resolved[0].plugins.len(), 1);
        assert_eq!(resolved[0].plugins[0].plugin.as_source(), "good-detect");
    }

    /// CEP 26 caps a package name at 64 characters.
    #[test]
    fn an_overlong_name_is_rejected() {
        let long = format!("__{}", "a".repeat(63));
        assert_eq!(long.len(), 65);
        assert!(!is_valid_virtual_package_name(&name(&long)));
        assert!(is_valid_virtual_package_name(&name(&long[..64])));
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
