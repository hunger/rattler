//! Saying what a plugin would have reported, without running it.
//!
//! CEP 30 lets `CONDA_OVERRIDE_<NAME>` stand in for a virtual package the client
//! detects itself. A plugin's virtual packages want the same thing, for stronger
//! reasons: detecting one can mean solving an environment, installing it and
//! running a program that talks to hardware, so a developer reproducing a bug on
//! a machine without that hardware has no other way to get the name.
//!
//! Two forms, the more specific winning:
//!
//! - `CONDA_OVERRIDE_FOOBAR` -- `__foobar`, whichever channel speaks for it.
//! - `CONDA_OVERRIDE_FOOBAR_CONDA_FORGE` -- `__foobar`, but only from the channel
//!   whose base URL ends in `conda-forge`.
//!
//! The channel-qualified form exists because the same name can come from
//! different channels' plugins in different views, and pinning all of them
//! together is not always what is wanted. It identifies a channel by the last
//! component of its base URL, which is short enough to type but *not* unique: a
//! mirror of a channel ends in the same component, and so does a label. That is
//! a deliberate trade -- see the note in the design document -- and it is why the
//! unqualified form exists at all.
//!
//! An override is read from a snapshot of the environment taken once per run, so
//! every plugin in one run agrees on what the overrides are, and so tests can
//! supply them without touching the process environment.

use std::collections::BTreeMap;

use rattler_conda_types::{
    ChannelUrl, GenericVirtualPackage, PackageName, ParseVersionError, SourcedVirtualPackage,
    Version, VirtualPackageSource,
};

/// The prefix CEP 30 gives these variables.
const PREFIX: &str = "CONDA_OVERRIDE_";

/// What an override says about one virtual package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Overridden {
    /// The name is present, with this value.
    Present(Box<GenericVirtualPackage>),

    /// The variable was set to the empty string, which CEP 30 uses to mean the
    /// name is not there. A plugin claiming it is not run and reports nothing.
    Absent,
}

/// An override was set but could not be read.
#[derive(Debug, thiserror::Error)]
#[error("the environment variable '{variable}' does not describe a virtual package")]
pub struct OverrideError {
    /// The variable that was set.
    pub variable: String,

    /// Why its value could not be used.
    #[source]
    pub source: ParseVersionError,
}

/// The `CONDA_OVERRIDE_*` variables in effect for one run.
///
/// A snapshot rather than a live read of the environment: a run resolves several
/// plugins, possibly concurrently, and they should not be able to disagree about
/// what was set.
#[derive(Clone, Debug, Default)]
pub struct PluginOverrides {
    variables: BTreeMap<String, String>,
}

impl PluginOverrides {
    /// Takes the overrides from this process's environment.
    pub fn from_env() -> Self {
        Self::from_variables(std::env::vars())
    }

    /// Takes the overrides from `variables`, ignoring anything not named like an
    /// override. For tests, and for a caller that keeps its own environment.
    pub fn from_variables(variables: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            variables: variables
                .into_iter()
                .filter(|(name, _)| name.starts_with(PREFIX))
                .collect(),
        }
    }

    /// What the environment says about `name` as `channel` would report it, or
    /// `None` if it says nothing.
    ///
    /// The channel-qualified variable wins over the general one, so pinning one
    /// channel does not require unpinning the rest.
    pub fn get(
        &self,
        name: &PackageName,
        channel: &ChannelUrl,
    ) -> Option<Result<Overridden, OverrideError>> {
        let qualified = channel_component(channel)
            .map(|component| format!("{}_{}", variable_name(name), shout(&component)));

        let (variable, value) = qualified
            .and_then(|variable| Some((variable.clone(), self.variables.get(&variable)?)))
            .or_else(|| {
                let variable = variable_name(name);
                let value = self.variables.get(&variable)?;
                Some((variable, value))
            })?;

        Some(parse(name, &variable, value))
    }

    /// The overrides that apply to `names` as `channel` would report them.
    ///
    /// A name missing from the result means the environment said nothing about
    /// it; a name mapped to [`Overridden::Absent`] means it said the name is not
    /// there. Those are different answers, which is why both are representable.
    ///
    /// When the result covers every name a plugin is on offer for, running that
    /// plugin cannot change the outcome and it is skipped.
    pub fn for_names<'a>(
        &self,
        names: impl IntoIterator<Item = &'a PackageName>,
        channel: &ChannelUrl,
    ) -> Result<BTreeMap<PackageName, Overridden>, OverrideError> {
        names
            .into_iter()
            .filter_map(|name| {
                let overridden = self.get(name, channel)?;
                Some(overridden.map(|overridden| (name.clone(), overridden)))
            })
            .collect()
    }
}

/// Overrides that name a package, as virtual packages attributed to the plugin
/// they stand in for.
///
/// An [`Overridden::Absent`] contributes nothing, which is what it means. The
/// source is [`VirtualPackageSource::Overridden`] rather than
/// [`Plugin`](VirtualPackageSource::Plugin): the value is visible exactly where
/// the plugin's verdict would have been, but no environment was built and
/// nothing may claim otherwise.
pub fn sourced(
    overridden: BTreeMap<PackageName, Overridden>,
    channel: &ChannelUrl,
    plugin: &PackageName,
) -> Vec<SourcedVirtualPackage> {
    overridden
        .into_values()
        .filter_map(|overridden| match overridden {
            Overridden::Present(package) => Some(*package),
            Overridden::Absent => None,
        })
        .map(|package| SourcedVirtualPackage {
            source: VirtualPackageSource::Overridden {
                channel: channel.clone(),
                plugin: plugin.clone(),
            },
            package,
        })
        .collect()
}

/// `__foobar` -> `CONDA_OVERRIDE_FOOBAR`.
fn variable_name(name: &PackageName) -> String {
    format!(
        "{PREFIX}{}",
        shout(name.as_normalized().trim_start_matches('_'))
    )
}

/// The last path component of a channel's base URL, which is what names it in an
/// override variable.
fn channel_component(channel: &ChannelUrl) -> Option<String> {
    channel
        .url()
        .path_segments()?
        .rfind(|segment| !segment.is_empty())
        .map(ToString::to_string)
}

/// Uppercased, with everything an environment variable cannot hold turned into
/// an underscore, so `conda-forge` reaches `CONDA_FORGE`.
fn shout(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// `<version>`, or `<version>=<build string>` to set both.
fn parse(name: &PackageName, variable: &str, value: &str) -> Result<Overridden, OverrideError> {
    if value.is_empty() {
        return Ok(Overridden::Absent);
    }

    let (version, build_string) = value.split_once('=').unwrap_or((value, "0"));
    let version = version.parse::<Version>().map_err(|source| OverrideError {
        variable: variable.to_string(),
        source,
    })?;

    Ok(Overridden::Present(Box::new(GenericVirtualPackage {
        name: name.clone(),
        version,
        build_string: build_string.to_string(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(variables: &[(&str, &str)]) -> PluginOverrides {
        PluginOverrides::from_variables(
            variables
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
        )
    }

    fn channel(url: &str) -> ChannelUrl {
        url::Url::parse(url).expect("a valid channel url").into()
    }

    fn name(name: &str) -> PackageName {
        PackageName::new_unchecked(name)
    }

    fn present(overridden: Option<Result<Overridden, OverrideError>>) -> String {
        match overridden.expect("an override was set").expect("it parses") {
            Overridden::Present(package) => format!("{}={}", package.version, package.build_string),
            Overridden::Absent => "absent".to_string(),
        }
    }

    #[test]
    fn a_name_is_overridden_for_every_channel() {
        let overrides = overrides(&[("CONDA_OVERRIDE_FOOBAR", "1.2.3")]);
        assert_eq!(
            present(overrides.get(&name("__foobar"), &channel("https://prefix.dev/org/"))),
            "1.2.3=0"
        );
    }

    /// The point of the qualified form: the same name from two channels, only
    /// one of them pinned.
    #[test]
    fn the_channel_qualified_form_wins_and_applies_only_there() {
        let overrides = overrides(&[
            ("CONDA_OVERRIDE_FOOBAR", "1.0.0"),
            ("CONDA_OVERRIDE_FOOBAR_CONDA_FORGE", "9.9.9"),
        ]);

        assert_eq!(
            present(overrides.get(
                &name("__foobar"),
                &channel("https://conda.anaconda.org/conda-forge/")
            )),
            "9.9.9=0",
            "the qualified variable wins for its channel"
        );
        assert_eq!(
            present(overrides.get(&name("__foobar"), &channel("https://prefix.dev/other/"))),
            "1.0.0=0",
            "and leaves every other channel on the general one"
        );
    }

    /// A channel name is not a valid variable name, so it is mangled the same way
    /// going in as a user would type it.
    #[test]
    fn a_channel_name_becomes_a_variable_name() {
        let overrides = overrides(&[("CONDA_OVERRIDE_FOOBAR_MY_CHANNEL", "2.0")]);
        assert_eq!(
            present(overrides.get(
                &name("__foobar"),
                &channel("https://prefix.dev/my-channel/")
            )),
            "2.0=0"
        );
    }

    /// A label and its parent share a last component with nothing else, so the
    /// label is what the variable names.
    #[test]
    fn a_label_is_named_by_its_own_last_component() {
        let overrides = overrides(&[("CONDA_OVERRIDE_FOOBAR_RC", "3.0")]);
        assert_eq!(
            present(overrides.get(
                &name("__foobar"),
                &channel("https://conda.anaconda.org/conda-forge/label/rc/")
            )),
            "3.0=0"
        );
        assert!(
            overrides
                .get(
                    &name("__foobar"),
                    &channel("https://conda.anaconda.org/conda-forge/")
                )
                .is_none(),
            "the parent channel is a different channel"
        );
    }

    /// CEP 30 uses an empty value to mean the name is not there, which is the
    /// only way to say "pretend this hardware is missing".
    #[test]
    fn an_empty_value_means_the_name_is_absent() {
        let overrides = overrides(&[("CONDA_OVERRIDE_FOOBAR", "")]);
        assert_eq!(
            present(overrides.get(&name("__foobar"), &channel("https://prefix.dev/org/"))),
            "absent"
        );
    }

    /// A build string matters for names like `__foobar_arch`, where the
    /// capability is in the build string rather than the version.
    #[test]
    fn a_build_string_can_be_set_too() {
        let overrides = overrides(&[("CONDA_OVERRIDE_FOOBAR_ARCH", "0=gen4")]);
        assert_eq!(
            present(overrides.get(&name("__foobar_arch"), &channel("https://prefix.dev/org/"))),
            "0=gen4"
        );
    }

    #[test]
    fn nothing_set_overrides_nothing() {
        let overrides = overrides(&[("PATH", "/usr/bin"), ("CONDA_OVERRIDE_OTHER", "1.0")]);
        assert!(
            overrides
                .get(&name("__foobar"), &channel("https://prefix.dev/org/"))
                .is_none()
        );
    }

    /// A value that is not a version is the user's mistake and must be said out
    /// loud: silently ignoring it would look like the override took effect.
    #[test]
    fn a_value_that_is_not_a_version_is_an_error() {
        let overrides = overrides(&[("CONDA_OVERRIDE_FOOBAR", "not a version")]);
        let error = overrides
            .get(&name("__foobar"), &channel("https://prefix.dev/org/"))
            .expect("an override was set")
            .expect_err("it does not parse");

        assert_eq!(error.variable, "CONDA_OVERRIDE_FOOBAR");
    }
}
