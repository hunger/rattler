//! A virtual package together with where it came from.

use rattler_digest::{Sha256Hash, serde::SerializableHash};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{ChannelUrl, GenericVirtualPackage, PackageName};

/// A virtual package and the source that produced it.
///
/// The source is not decoration. Virtual packages are scoped to a *view* -- a
/// channel together with every channel it reaches through a CEP-42 `base` chain
/// -- and the source is what says which views a given value belongs to. Two
/// independent channels may each detect a different `__rocm`, and without the
/// source travelling alongside there would be nowhere to record which of them a
/// verdict answers for; the two would collapse into one the moment they were put
/// in a list together.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct SourcedVirtualPackage {
    /// Where this came from, and therefore which views it is visible in.
    pub source: VirtualPackageSource,

    /// The virtual package itself, as handed to the solver.
    pub package: GenericVirtualPackage,
}

/// Where a virtual package came from.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualPackageSource {
    /// Detected by this client itself.
    ///
    /// Belongs to no channel and is visible in every view: CEP 30 makes the
    /// standard virtual packages an obligation of the client rather than
    /// something a channel provides, so a built-in cannot be missing from a view
    /// however the channels are configured.
    BuiltIn,

    /// Detected by a plugin a channel registered.
    ///
    /// Visible in that channel's view, and in the view of any channel that
    /// reaches it through a `base` chain.
    Plugin {
        /// The channel that registered the plugin.
        channel: ChannelUrl,

        /// The package providing the plugin.
        plugin: PackageName,

        /// Identifies the exact plugin build that produced this: a hash over the
        /// plugin package *and* every package in the environment it ran in, so
        /// it changes when any dependency of the plugin changes.
        #[serde_as(as = "SerializableHash::<rattler_digest::Sha256>")]
        environment: Sha256Hash,
    },
}

impl VirtualPackageSource {
    /// The channel that provided this, or `None` for a built-in.
    pub fn channel(&self) -> Option<&ChannelUrl> {
        match self {
            Self::BuiltIn => None,
            Self::Plugin { channel, .. } => Some(channel),
        }
    }

    /// Whether this came from the client rather than from a channel.
    ///
    /// A built-in is the weakest source: a plugin claiming the same name
    /// overrides it, since CEP 30 requires the name to be *present* and does not
    /// dictate that the client's own detection is what fills it.
    pub fn is_built_in(&self) -> bool {
        match self {
            Self::BuiltIn => true,
            Self::Plugin { .. } => false,
        }
    }
}
