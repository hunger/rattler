#[cfg(feature = "experimental-virtual-package-plugins")]
use indexmap::IndexMap;
#[cfg(feature = "experimental-virtual-package-plugins")]
use itertools::Itertools;
use miette::IntoDiagnostic;
#[cfg(not(feature = "experimental-virtual-package-plugins"))]
use rattler_conda_types::GenericVirtualPackage;
#[cfg(feature = "experimental-virtual-package-plugins")]
use rattler_conda_types::{Channel, ChannelConfig, PackageName, Platform};
#[cfg(feature = "experimental-virtual-package-plugins")]
use rattler_repodata_gateway::Gateway;
/// The names every client detects itself. Owned by the plugins crate, which
/// needs the same list to say what its built-in factory speaks for.
#[cfg(feature = "experimental-virtual-package-plugins")]
use rattler_virtual_package_plugins::{STANDARDIZED_VIRTUAL_PACKAGES, channel_view};

/// Print detected virtual packages.
#[derive(Debug, clap::Parser)]
#[cfg_attr(
    feature = "experimental-virtual-package-plugins",
    clap(after_help = r#"Examples:
  rattler virtual-packages
  rattler virtual-packages -c ./test-data/channels/virtual-package-plugins
  rattler virtual-packages -c ./test-data/channels/virtual-package-plugins-derived
  rattler virtual-packages -c ./test-data/channels/virtual-package-plugins --detect"#)
)]
pub struct Opt {
    /// Channels to list registered virtual package plugins for
    #[cfg(feature = "experimental-virtual-package-plugins")]
    #[clap(short, long)]
    channels: Vec<String>,

    /// Platforms to read registrations for [default: current and noarch]
    #[cfg(feature = "experimental-virtual-package-plugins")]
    #[clap(short, long)]
    platforms: Vec<Platform>,

    /// Run each registered plugin and report the virtual packages it detects
    #[cfg(feature = "experimental-virtual-package-plugins")]
    #[clap(long)]
    detect: bool,

    /// Seconds a plugin may run before it is killed [default: 5, maximum: 60]
    #[cfg(feature = "experimental-virtual-package-plugins")]
    #[clap(long, requires = "detect")]
    plugin_timeout: Option<u64>,

    /// Report how long each stage of detection took
    #[cfg(feature = "experimental-virtual-package-plugins")]
    #[clap(long, requires = "detect")]
    timings: bool,
}

pub async fn virtual_packages(opt: Opt, offline: bool) -> miette::Result<()> {
    #[cfg(feature = "experimental-virtual-package-plugins")]
    {
        use rattler_virtual_package_plugins::{BuiltinVirtualPackages, VirtualPackageFactory};

        // Detected once and passed on: the same set is printed here and offered
        // to every view, and detecting can mean a driver query. The factory is
        // the only thing that reads `CONDA_OVERRIDE_*` for these.
        let built_in = BuiltinVirtualPackages::from_env()
            .resolve()
            .await
            .map_err(|err| miette::miette!(err))?;
        for detected in &built_in {
            println!("{}", detected.package);
        }

        if opt.detect {
            let timeout = opt.plugin_timeout.map_or_else(
                rattler_virtual_package_plugins::RunTimeout::default,
                |seconds| {
                    rattler_virtual_package_plugins::RunTimeout::new(
                        std::time::Duration::from_secs(seconds),
                    )
                },
            );
            detect_plugins(
                &opt.channels,
                &opt.platforms,
                offline,
                timeout,
                opt.timings,
                &built_in,
            )
            .await?;
        } else {
            print_plugins(&opt.channels, &opt.platforms, offline).await?;
        }
    }

    #[cfg(not(feature = "experimental-virtual-package-plugins"))]
    {
        let _ = (opt, offline);
        let detected = rattler_virtual_packages::VirtualPackage::detect(
            &rattler_virtual_packages::VirtualPackageOverrides::from_env(),
        )
        .into_diagnostic()?;
        for package in detected {
            println!("{}", GenericVirtualPackage::from(package));
        }
    }

    Ok(())
}

/// A gateway configured for reading plugin registrations: no sharded repodata
/// preference, and the caller's offline setting respected.
#[cfg(feature = "experimental-virtual-package-plugins")]
fn plugin_gateway(offline: bool) -> miette::Result<Gateway> {
    use std::collections::HashMap;

    use rattler_repodata_gateway::SourceConfig;

    Ok(Gateway::builder()
        .with_client(super::client::create_client_with_middleware(offline)?)
        .with_channel_config(rattler_repodata_gateway::ChannelConfig {
            default: SourceConfig {
                cache_action: super::client::repodata_cache_action(offline),
                ..SourceConfig::default()
            },
            per_channel: HashMap::new(),
        })
        .finish())
}

/// Prints the plugin registrations declared by each `(channel, platform)`
/// subdirectory, in the order the channels were given.
#[cfg(feature = "experimental-virtual-package-plugins")]
async fn print_plugins(
    channels: &[String],
    platforms: &[Platform],
    offline: bool,
) -> miette::Result<()> {
    use std::env;

    if channels.is_empty() {
        return Ok(());
    }

    let channel_config =
        ChannelConfig::default_with_root_dir(env::current_dir().into_diagnostic()?);
    let channels = channels
        .iter()
        .map(|channel| Channel::from_str(channel, &channel_config))
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()?;

    let platforms = if platforms.is_empty() {
        vec![Platform::current(), Platform::NoArch]
    } else {
        platforms.to_vec()
    };

    let gateway = plugin_gateway(offline)?;

    for channel in &channels {
        for platform in &platforms {
            let view = channel_view(&gateway, channel, *platform)
                .await
                .into_diagnostic()?;
            let chain: Vec<_> = view.chain.into_iter().map(Channel::from_url).collect();
            let claims = collect_claims(&gateway, &chain, *platform).await?;
            if claims.is_empty() {
                continue;
            }

            println!(
                "\n{}{} {}",
                console::Emoji("🔌 ", ""),
                console::style(channel.canonical_name()).bold(),
                console::style(format!("[{platform}]")).dim(),
            );
            for (virtual_package, claims) in &claims {
                println!(
                    "  {} {} {}",
                    console::style(console::Emoji("•", "-")).cyan(),
                    console::style(virtual_package.as_source()).bold(),
                    console::style(format!(
                        "from {}",
                        claims.iter().map(Claim::to_string).join(", ")
                    ))
                    .dim(),
                );
            }

            for warning in override_warnings(&claims) {
                println!(
                    "  {} {}",
                    console::style(console::Emoji("⚠", "!")).yellow(),
                    console::style(warning).yellow(),
                );
            }
        }
    }

    Ok(())
}

/// One channel's registration of one virtual package. `depth` is the distance
/// along `base` edges from the channel asked about, so `0` is its own claim.
#[cfg(feature = "experimental-virtual-package-plugins")]
struct Claim {
    channel: String,
    plugin: PackageName,
    depth: usize,
}

#[cfg(feature = "experimental-virtual-package-plugins")]
impl std::fmt::Display for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.plugin.as_source(), self.channel)
    }
}

/// Every virtual package the chain registers, mapped to the claims on it in
/// chain order, so a name claimed more than once carries all its claimants.
#[cfg(feature = "experimental-virtual-package-plugins")]
async fn collect_claims(
    gateway: &Gateway,
    chain: &[Channel],
    platform: Platform,
) -> miette::Result<IndexMap<PackageName, Vec<Claim>>> {
    let mut claims: IndexMap<_, Vec<Claim>> = IndexMap::new();

    for (depth, channel) in chain.iter().enumerate() {
        let plugins = gateway
            .virtual_package_plugins(channel, platform)
            .await
            .into_diagnostic()?;
        for (plugin, provided) in plugins {
            for virtual_package in provided {
                claims.entry(virtual_package).or_default().push(Claim {
                    channel: short_channel_name(channel),
                    plugin: plugin.clone(),
                    depth,
                });
            }
        }
    }

    claims.sort_keys();
    Ok(claims)
}

/// Warnings for claims that shadow something already provided: a name a client
/// detects itself, or one an inherited channel already registers.
///
/// What should happen instead is undecided (open question 7 of the plugin
/// proposal), so this only reports.
#[cfg(feature = "experimental-virtual-package-plugins")]
fn override_warnings(claims: &IndexMap<PackageName, Vec<Claim>>) -> Vec<String> {
    let mut warnings = Vec::new();

    for (virtual_package, claims) in claims {
        let name = virtual_package.as_source();

        if STANDARDIZED_VIRTUAL_PACKAGES.contains(&name) {
            warnings.push(format!(
                "{name} is a standardized virtual package that clients detect themselves, \
                 but it is registered by {}",
                claims.iter().map(Claim::to_string).join(", ")
            ));
        }

        // Claims are in chain order, so anything past the first is inherited
        // from further along `base` and is what the earlier claim shadows.
        if let Some((first, shadowed)) = claims.split_first()
            && !shadowed.is_empty()
        {
            warnings.push(format!(
                "{name} is registered by {first}, overriding {}",
                shadowed
                    .iter()
                    .map(|claim| if claim.depth == first.depth {
                        format!("{claim} in the same channel")
                    } else {
                        format!("{claim} it inherits from")
                    })
                    .join(", ")
            ));
        }
    }

    warnings
}

#[cfg(all(test, feature = "experimental-virtual-package-plugins"))]
mod tests {
    use super::*;

    /// The claim on a name registered by both a channel and the channel it
    /// inherits from is reported as an override, and a lone claim is not.
    #[test]
    fn only_shadowed_claims_warn() {
        let claim = |channel: &str, plugin: &str, depth| Claim {
            channel: channel.to_string(),
            plugin: PackageName::new_unchecked(plugin),
            depth,
        };
        let claims = IndexMap::from([
            (
                PackageName::new_unchecked("__rocm"),
                vec![claim("derived", "rocm-detect", 0)],
            ),
            (
                PackageName::new_unchecked("__vendor"),
                vec![
                    claim("derived", "vendor-detect", 0),
                    claim("base", "base-detect", 1),
                ],
            ),
        ]);

        let warnings = override_warnings(&claims);
        assert_eq!(warnings.len(), 1, "got {warnings:#?}");
        assert!(
            warnings[0].contains("__vendor")
                && warnings[0].contains("overriding")
                && warnings[0].contains("inherits from"),
            "{}",
            warnings[0]
        );
    }
}

/// Runs the plugins the given channels register and reports what each detected.
///
/// Where two channels claim one virtual package, the higher-priority channel
/// wins and the other plugin is reported as shadowed rather than run. Channels
/// are in the priority order they were given on the command line.
///
/// A plugin that fails is reported and skipped rather than aborting the run: one
/// broken plugin should not hide what the others found, and a system without the
/// hardware is indistinguishable from a broken plugin at this level.
#[cfg(feature = "experimental-virtual-package-plugins")]
async fn detect_plugins(
    channels: &[String],
    platforms: &[Platform],
    offline: bool,
    timeout: rattler_virtual_package_plugins::RunTimeout,
    show_timings: bool,
    built_in: &[rattler_conda_types::SourcedVirtualPackage],
) -> miette::Result<()> {
    use std::env;

    use rattler_cache::{
        default_cache_dir, package_cache::PackageCache,
        virtual_package_plugin_cache::VirtualPackagePluginCache,
    };
    use rattler_repodata_gateway::SubdirVirtualPackagePlugins;
    use rattler_virtual_package_plugins::{
        DetectOptions, PluginContext, PluginOverrides, combine, detect_virtual_packages, overrides,
        resolve_views,
    };

    if channels.is_empty() {
        return Ok(());
    }

    let channel_config =
        ChannelConfig::default_with_root_dir(env::current_dir().into_diagnostic()?);
    let channels = channels
        .iter()
        .map(|channel| Channel::from_str(channel, &channel_config))
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()?;

    // Detection inspects this machine, so only the host platform is meaningful.
    let platform = match platforms {
        [] => Platform::current(),
        [platform] => *platform,
        _ => miette::bail!("--detect works on one platform at a time"),
    };

    let cache_dir = default_cache_dir()
        .map_err(|e| miette::miette!("could not determine cache directory: {e}"))?;
    rattler_cache::ensure_cache_dir(&cache_dir)
        .map_err(|e| miette::miette!("could not create cache directory: {e}"))?;
    let gateway = plugin_gateway(offline)?;
    let package_cache = PackageCache::new(cache_dir.join(rattler_cache::PACKAGE_CACHE_DIR));
    let detection_cache = VirtualPackagePluginCache::new(
        cache_dir.join(rattler_cache::VIRTUAL_PACKAGE_PLUGINS_CACHE_DIR),
    );
    let environment_root = cache_dir.join(rattler_cache::EXEC_ENVS_DIR).join("plugins");
    // One timestamp for the whole run, so every plugin agrees on what now is.
    let now = jiff::Timestamp::now().as_second();

    // One view per channel asked about: the channel plus everything it inherits
    // through a `base` chain. Views are independent, so two unrelated channels
    // each answer for their own names rather than one shadowing the other.
    let mut views = Vec::new();
    for channel in &channels {
        views.push(
            channel_view(&gateway, channel, platform)
                .await
                .into_diagnostic()?,
        );
    }

    // Every channel any view can see, not just the ones named on the command
    // line: a view inherits its base channels' registrations, and a base is
    // usually not something the user listed. Deduplicated, since views overlap
    // wherever they share a base.
    let mut in_scope: Vec<Channel> = Vec::new();
    for url in views.iter().flat_map(|view| &view.chain) {
        if !in_scope.iter().any(|channel| channel.base_url == *url) {
            in_scope.push(Channel::from_url(url.clone()));
        }
    }

    let mut registrations = Vec::new();
    for channel in &in_scope {
        let plugins = gateway
            .virtual_package_plugins(channel, platform)
            .await
            .into_diagnostic()?;
        if !plugins.is_empty() {
            registrations.push(SubdirVirtualPackagePlugins {
                channel: channel.base_url.clone(),
                platform,
                plugins,
            });
        }
    }
    let resolved_views =
        resolve_views(&views, registrations).map_err(|err| miette::miette!(err))?;

    let overrides = PluginOverrides::from_env();
    let context = PluginContext {
        gateway: &gateway,
        package_cache: &package_cache,
        detection_cache: &detection_cache,
        environment_root: &environment_root,
        host_platform: platform,
        timeout,
        now,
        overrides: &overrides,
    };

    for view in &resolved_views {
        if view.plugins.is_empty() && view.shadowed.is_empty() {
            continue;
        }

        println!(
            "\n{}{} {}",
            console::Emoji("🔌 ", ""),
            console::style(&view.channel).bold(),
            console::style(format!("[{platform}]")).dim(),
        );

        // Which built-ins survive depends on what the plugins produced, not on
        // what they claimed, so this is collected as they run.
        let mut produced: Vec<rattler_conda_types::SourcedVirtualPackage> = Vec::new();

        for resolved in view.plugins.iter().chain(&view.shadowed) {
            if resolved.provides.is_empty() {
                report_shadowed(resolved);
                continue;
            }

            let channel = in_scope
                .iter()
                .find(|channel| channel.base_url == resolved.channel)
                .expect("a resolved plugin comes from a channel some view can see");

            // An override stands in for the plugin's verdict. When it covers
            // every name the plugin is on offer for, running it -- solving an
            // environment, installing it, starting a process -- cannot change
            // the answer.
            let overridden = context
                .overrides
                .for_names(&resolved.provides, &resolved.channel)
                .map_err(|err| miette::miette!(err))?;
            if overridden.len() == resolved.provides.len() {
                let stood_in_for =
                    overrides::sourced(overridden, &resolved.channel, &resolved.plugin);
                report_overridden(resolved, &stood_in_for);
                produced.extend(stood_in_for);
                continue;
            }

            let detection = detect_virtual_packages(DetectOptions {
                gateway: context.gateway,
                package_cache: context.package_cache,
                detection_cache: context.detection_cache,
                channel,
                plugin: &resolved.plugin,
                declared: &resolved.declared,
                environment_root: context.environment_root,
                host_platform: context.host_platform,
                timeout: context.timeout,
                now: context.now,
            })
            .await;

            match detection {
                Ok(detection) => {
                    produced.extend(
                        detection
                            .virtual_packages
                            .iter()
                            .filter(|detected| resolved.provides.contains(&detected.package.name))
                            .filter(|detected| !overridden.contains_key(&detected.package.name))
                            .cloned(),
                    );
                    report_detection(resolved, &detection, &overridden, show_timings);
                    produced.extend(overrides::sourced(
                        overridden,
                        &resolved.channel,
                        &resolved.plugin,
                    ));
                }
                Err(err) => {
                    println!(
                        "  {} {} {}",
                        console::style(console::Emoji("✖", "x")).red(),
                        console::style(resolved.plugin.as_source()).bold(),
                        console::style("(skipped)").dim(),
                    );
                    for line in explain(&err) {
                        println!("      {}", console::style(line).red());
                    }
                    // The plugin's own account of what went wrong, which is
                    // usually more specific than anything this side can say.
                    for line in err.plugin_stderr().into_iter().flat_map(str::lines) {
                        println!("      {}", console::style(line).dim());
                    }
                }
            }
        }

        // `combine` is what keeps a name CEP 30 mandates from vanishing when a
        // plugin claims it and comes back empty.
        for detected in combine(built_in, produced)
            .into_iter()
            .filter(|detected| detected.source.is_built_in())
        {
            println!(
                "  {} {} {}",
                console::style(console::Emoji("•", "-")).dim(),
                console::style(&detected.package).dim(),
                console::style("(built in)").dim(),
            );
        }
    }

    Ok(())
}

/// Reports what one plugin detected, and what of it a higher-priority channel
/// already spoke for.
#[cfg(feature = "experimental-virtual-package-plugins")]
fn report_detection(
    resolved: &rattler_virtual_package_plugins::ResolvedPlugin,
    detection: &rattler_virtual_package_plugins::Detection,
    overridden: &std::collections::BTreeMap<
        PackageName,
        rattler_virtual_package_plugins::Overridden,
    >,
    show_timings: bool,
) {
    let source = if detection.from_cache {
        "from cache"
    } else {
        "ran the plugin"
    };
    println!(
        "  {} {} {}",
        console::style(console::Emoji("✔", "+")).green(),
        console::style(resolved.plugin.as_source()).bold(),
        console::style(format!("({source})")).dim(),
    );

    if show_timings {
        let timings = &detection.timings;
        println!(
            "      {}",
            console::style(format!(
                "repodata {:?}{}, solve {:?}, install {:?}, run {:?}",
                timings.environment.repodata,
                if timings.environment.refetched_for_dependencies {
                    " (two queries: the plugin has dependencies)"
                } else {
                    ""
                },
                timings.environment.solve,
                timings.environment.install,
                timings.run,
            ))
            .cyan(),
        );
    }

    let used: Vec<_> = detection
        .virtual_packages
        .iter()
        .filter(|detected| resolved.provides.contains(&detected.package.name))
        .filter(|detected| !overridden.contains_key(&detected.package.name))
        .collect();
    if used.is_empty() && overridden.is_empty() {
        println!(
            "      {}",
            console::style(format!(
                "none of {} are present on this system",
                resolved
                    .provides
                    .iter()
                    .map(PackageName::as_source)
                    .join(", ")
            ))
            .dim(),
        );
    }
    for detected in used {
        println!("      {}", console::style(&detected.package).green());
    }

    // Whatever the plugin said about an overridden name, the environment is what
    // counts. Saying so beats printing a value the solver will not see.
    for (name, overridden) in overridden {
        let line = match overridden {
            rattler_virtual_package_plugins::Overridden::Present(package) => {
                format!("{package} (overridden)")
            }
            rattler_virtual_package_plugins::Overridden::Absent => {
                format!("{} overridden to absent", name.as_source())
            }
        };
        println!("      {}", console::style(line).yellow());
    }

    // A verdict this plugin gave that another channel's plugin speaks for. It
    // still had to give one, and saying so beats a silently missing line.
    for (virtual_package, winner) in &resolved.shadowed_by {
        println!(
            "      {}",
            console::style(format!(
                "{} is provided by {winner}",
                virtual_package.as_source()
            ))
            .dim(),
        );
    }
}

/// Reports a registration that is not run because the environment already says
/// what it would have reported.
#[cfg(feature = "experimental-virtual-package-plugins")]
fn report_overridden(
    resolved: &rattler_virtual_package_plugins::ResolvedPlugin,
    stood_in_for: &[rattler_conda_types::SourcedVirtualPackage],
) {
    println!(
        "  {} {} {}",
        console::style(console::Emoji("⇄", "=")).yellow(),
        console::style(resolved.plugin.as_source()).bold(),
        console::style("(overridden, not run)").dim(),
    );
    for detected in stood_in_for {
        println!("      {}", detected.package);
    }
    // A name overridden to absent is reported nowhere else, and silence would
    // read as the plugin having found nothing rather than as an instruction.
    for name in resolved
        .provides
        .iter()
        .filter(|name| !stood_in_for.iter().any(|d| &&d.package.name == name))
    {
        println!(
            "      {}",
            console::style(format!("{} overridden to absent", name.as_source())).dim(),
        );
    }
}

/// Reports a registration that is not run because another channel speaks for
/// everything it claimed.
#[cfg(feature = "experimental-virtual-package-plugins")]
fn report_shadowed(resolved: &rattler_virtual_package_plugins::ResolvedPlugin) {
    println!(
        "  {} {} {}",
        console::style(console::Emoji("○", "-")).dim(),
        console::style(resolved.plugin.as_source()).bold(),
        console::style("(not run)").dim(),
    );
    for (virtual_package, winner) in &resolved.shadowed_by {
        println!(
            "      {}",
            console::style(format!(
                "{} is provided by {winner}",
                virtual_package.as_source()
            ))
            .dim(),
        );
    }
}

/// The message of an error and of every cause beneath it.
///
/// A detection failure is usually reported by an outer layer -- "could not
/// prepare the environment" -- while the useful part is further down, so
/// printing only the top message throws away the answer to "why".
#[cfg(feature = "experimental-virtual-package-plugins")]
fn explain(err: &dyn std::error::Error) -> Vec<String> {
    let mut lines = vec![err.to_string()];
    let mut source = err.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        // thiserror's `#[error(transparent)]` repeats the message it wraps.
        if lines.last() != Some(&message) {
            lines.push(message);
        }
        source = cause.source();
    }
    lines
}

/// A channel name short enough to repeat on every line.
///
/// A channel's canonical name is already short when it has an alias
/// (`conda-forge`), but a local one is a whole `file://` URL. Detail lines repeat
/// the channel once per claim, so the last path segment is used there; the header
/// still prints the full name, which is what identifies it unambiguously.
#[cfg(feature = "experimental-virtual-package-plugins")]
fn short_channel_name(channel: &Channel) -> String {
    let canonical = channel.canonical_name();
    if channel.base_url.url().scheme() != "file" {
        return canonical;
    }
    channel
        .base_url
        .url()
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map_or(canonical, ToString::to_string)
}
