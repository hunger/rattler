#[cfg(feature = "experimental-virtual-package-plugins")]
use indexmap::IndexMap;
#[cfg(feature = "experimental-virtual-package-plugins")]
use itertools::Itertools;
use miette::IntoDiagnostic;
use rattler_conda_types::GenericVirtualPackage;
#[cfg(feature = "experimental-virtual-package-plugins")]
use rattler_conda_types::{Channel, ChannelConfig, PackageName, Platform};
#[cfg(feature = "experimental-virtual-package-plugins")]
use rattler_repodata_gateway::{
    DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH, Gateway, resolve_channel_relation,
};
use rattler_virtual_packages::VirtualPackageOverrides;

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
}

pub async fn virtual_packages(opt: Opt, offline: bool) -> miette::Result<()> {
    let virtual_packages =
        rattler_virtual_packages::VirtualPackage::detect(&VirtualPackageOverrides::from_env())
            .into_diagnostic()?;
    for package in virtual_packages {
        println!("{}", GenericVirtualPackage::from(package.clone()));
    }

    #[cfg(feature = "experimental-virtual-package-plugins")]
    if opt.detect {
        detect_plugins(&opt.channels, &opt.platforms, offline).await?;
    } else {
        print_plugins(&opt.channels, &opt.platforms, offline).await?;
    }

    #[cfg(not(feature = "experimental-virtual-package-plugins"))]
    let _ = (opt, offline);

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
            let chain = base_chain(&gateway, channel, *platform).await?;
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

/// Virtual packages every client detects itself, so a plugin registering one
/// shadows a name the solver already fills in.
///
/// Names rather than a rattler API because [`rattler_virtual_packages`] exposes
/// no enumeration: they live in the `From<VirtualPackage> for
/// GenericVirtualPackage` impls. `standardized_names_stay_in_sync` guards the
/// drift.
#[cfg(feature = "experimental-virtual-package-plugins")]
const STANDARDIZED_VIRTUAL_PACKAGES: &[&str] = &[
    "__unix",
    "__linux",
    "__win",
    "__osx",
    "__ios",
    "__android",
    "__glibc",
    "__musl",
    "__eglibc",
    "__cuda",
    "__cuda_arch",
    "__archspec",
];

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

/// The chain of channels `channel` inherits virtual packages from: itself
/// first, then each [CEP-42] `base` in turn.
///
/// Only `base` is followed. It names the channel of higher priority that the
/// declaring channel builds on, so its virtual packages are in scope; an
/// `overrides` edge points the other way, at a channel being superseded.
///
/// References resolve through [`resolve_channel_relation`], so a reference the
/// gateway would refuse is skipped here too, and one already in the chain ends
/// the walk, which terminates a `base` cycle. The depth cap is CEP-42's own
/// [`DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH`].
///
/// [CEP-42]: https://github.com/conda/ceps/blob/main/cep-0042.md
#[cfg(feature = "experimental-virtual-package-plugins")]
async fn base_chain(
    gateway: &Gateway,
    channel: &Channel,
    platform: Platform,
) -> miette::Result<Vec<Channel>> {
    let mut chain = vec![channel.clone()];
    let mut seen = std::collections::HashSet::from([channel.base_url.clone()]);

    while chain.len() <= DEFAULT_CHANNEL_RELATIONS_MAX_DEPTH {
        let declaring = chain.last().expect("seeded above");
        let Some(relations) = gateway
            .channel_relations(declaring, platform)
            .await
            .into_diagnostic()?
        else {
            break;
        };
        let Some(base) = relations
            .base
            .as_deref()
            .and_then(|base| resolve_channel_relation(&declaring.base_url, base))
        else {
            break;
        };
        if !seen.insert(base.clone()) {
            break;
        }
        chain.push(Channel::from_url(base));
    }

    Ok(chain)
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
    use rattler_virtual_packages::{VirtualPackageOverrides, VirtualPackages};

    use super::*;

    /// Guards [`STANDARDIZED_VIRTUAL_PACKAGES`] against rattler gaining a
    /// virtual package it detects that this list doesn't know about -- an
    /// unlisted name means a plugin could shadow it without a warning.
    ///
    /// Only covers names that appear in per-platform detection, so ones that
    /// are never a default (`__cuda`, `__cuda_arch`, and the non-glibc libc
    /// flavors) still have to be added by hand.
    #[test]
    fn standardized_names_stay_in_sync() {
        let overrides = VirtualPackageOverrides::default();
        for platform in [
            Platform::Linux64,
            Platform::LinuxAarch64,
            Platform::Osx64,
            Platform::OsxArm64,
            Platform::Win64,
            Platform::EmscriptenWasm32,
        ] {
            let detected = VirtualPackages::detect_for_platform(platform, &overrides)
                .expect("detection for a known platform");
            for package in detected.into_generic_virtual_packages() {
                let name = package.name.as_source().to_string();
                assert!(
                    STANDARDIZED_VIRTUAL_PACKAGES.contains(&name.as_str()),
                    "{platform} detects {name}, which STANDARDIZED_VIRTUAL_PACKAGES omits"
                );
            }
        }
    }

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

/// Runs every plugin the given channels register and reports what each detected.
///
/// A plugin that fails is reported and skipped rather than aborting the run: one
/// broken plugin should not hide what the others found, and a system without the
/// hardware is indistinguishable from a broken plugin at this level.
#[cfg(feature = "experimental-virtual-package-plugins")]
async fn detect_plugins(
    channels: &[String],
    platforms: &[Platform],
    offline: bool,
) -> miette::Result<()> {
    use std::{collections::BTreeSet, env};

    use rattler_cache::{
        default_cache_dir, package_cache::PackageCache,
        virtual_package_plugin_cache::VirtualPackagePluginCache,
    };
    use rattler_virtual_package_plugins::{DetectOptions, detect_virtual_packages};

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

    for channel in &channels {
        let registrations = gateway
            .virtual_package_plugins(channel, platform)
            .await
            .into_diagnostic()?;
        if registrations.is_empty() {
            continue;
        }

        println!(
            "\n{}{} {}",
            console::Emoji("🔌 ", ""),
            console::style(channel.canonical_name()).bold(),
            console::style(format!("[{platform}]")).dim(),
        );

        for (plugin, declared) in &registrations {
            let declared: BTreeSet<_> = declared.iter().cloned().collect();
            let detection = detect_virtual_packages(DetectOptions {
                gateway: &gateway,
                package_cache: &package_cache,
                detection_cache: &detection_cache,
                channel,
                plugin,
                declared: &declared,
                environment_root: &environment_root,
                host_platform: platform,
                now,
            })
            .await;

            match detection {
                Ok(detection) => {
                    let source = if detection.from_cache {
                        "from cache"
                    } else {
                        "ran the plugin"
                    };
                    println!(
                        "  {} {} {}",
                        console::style(console::Emoji("✔", "+")).green(),
                        console::style(plugin.as_source()).bold(),
                        console::style(format!("({source})")).dim(),
                    );
                    if detection.virtual_packages.is_empty() {
                        println!(
                            "      {}",
                            console::style(format!(
                                "none of {} are present on this system",
                                declared.iter().map(PackageName::as_source).join(", ")
                            ))
                            .dim(),
                        );
                    }
                    for detected in &detection.virtual_packages {
                        println!("      {}", console::style(&detected.package).green());
                    }
                }
                Err(err) => {
                    println!(
                        "  {} {} {}",
                        console::style(console::Emoji("✖", "x")).red(),
                        console::style(plugin.as_source()).bold(),
                        console::style("(skipped)").dim(),
                    );
                    for line in explain(&err) {
                        println!("      {}", console::style(line).red());
                    }
                }
            }
        }
    }

    Ok(())
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
