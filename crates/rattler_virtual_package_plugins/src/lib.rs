//! Detect virtual packages using detection plugins that a conda channel
//! registers in its repodata.
//!
//! **Experimental.** The design, including the parts not implemented yet, is
//! written up in `docs/virtual-package-plugins.md` next to this crate.
//!
//! A channel registers a plugin package and the virtual packages it speaks for.
//! Detecting them means installing that plugin into an environment of its own,
//! running it, and reading its verdicts back. This crate currently implements
//! the parts that need no I/O:
//!
//! - [`protocol`] parses what a plugin writes to stdout.
//! - [`contract`] checks those verdicts against what the channel registered.
//! - [`runner`] runs a plugin out of an environment that already exists.
//!
//! Nothing here fetches or installs a plugin yet.

#![deny(missing_docs)]

#[cfg(feature = "experimental-virtual-package-plugins")]
pub mod contract;
#[cfg(feature = "experimental-virtual-package-plugins")]
pub mod protocol;
#[cfg(feature = "experimental-virtual-package-plugins")]
pub mod runner;

#[cfg(feature = "experimental-virtual-package-plugins")]
pub use contract::{ContractViolation, validate};
#[cfg(feature = "experimental-virtual-package-plugins")]
pub use protocol::{
    CachePolicy, Detected, PluginLine, PluginOutput, ProtocolError, Verdict, parse_output,
};
#[cfg(feature = "experimental-virtual-package-plugins")]
pub use runner::{PluginRun, RunnerError, run_plugin};
