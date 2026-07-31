//! Detect virtual packages using detection plugins that a conda channel
//! registers in its repodata.
//!
//! **Experimental.** The design, including the parts not implemented yet, is
//! written up in `docs/virtual-package-plugins.md` next to this crate.
//!
//! A channel registers a plugin package and the virtual packages it speaks for.
//! Detecting them means installing that plugin into an environment of its own,
//! running it, and reading its verdicts back:
//!
//! - [`environment`] installs a plugin into an environment of its own.
//! - [`runner`] runs a plugin out of that environment.
//! - [`protocol`] parses what a plugin writes to stdout.
//! - [`contract`] checks those verdicts against what the channel registered.
//! - [`detect`] composes all of those with a cache and returns
//!   [`ChannelVirtualPackage`](rattler_conda_types::ChannelVirtualPackage)s.
//!
//! [`detect::detect_virtual_packages`] is the entry point. The rest is public
//! because it is useful on its own to a caller that wants to do part of this
//! itself.

#![deny(missing_docs)]

#[cfg(feature = "experimental-virtual-package-plugins")]
pub mod contract;
#[cfg(feature = "experimental-virtual-package-plugins")]
pub mod detect;
#[cfg(feature = "experimental-virtual-package-plugins")]
pub mod environment;
#[cfg(feature = "experimental-virtual-package-plugins")]
pub mod protocol;
#[cfg(feature = "experimental-virtual-package-plugins")]
pub mod runner;

#[cfg(feature = "experimental-virtual-package-plugins")]
pub use contract::{ContractViolation, validate};
#[cfg(feature = "experimental-virtual-package-plugins")]
pub use detect::{DetectError, DetectOptions, Detection, detect_virtual_packages};
#[cfg(feature = "experimental-virtual-package-plugins")]
pub use environment::{
    EnvironmentError, PluginEnvironment, PluginEnvironmentOptions, ensure_plugin_environment,
    environment_sha256,
};
#[cfg(feature = "experimental-virtual-package-plugins")]
pub use protocol::{
    CachePolicy, Detected, PluginLine, PluginOutput, ProtocolError, Verdict, parse_output,
};
#[cfg(feature = "experimental-virtual-package-plugins")]
pub use runner::{PluginRun, RunnerError, run_plugin};
