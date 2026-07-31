//! A virtual package attributed to the channel whose plugin detected it.

use rattler_digest::{Sha256Hash, serde::SerializableHash};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{ChannelUrl, GenericVirtualPackage};

/// A virtual package detected by a channel's detection plugin, together with
/// enough provenance to tell two channels' claims on the same name apart and to
/// know which plugin build produced it.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ChannelVirtualPackage {
    /// The channel that registered the plugin that produced this.
    pub channel: ChannelUrl,

    /// Identifies the exact plugin build that produced this: a hash over the
    /// plugin package *and* every package in the environment it ran in, so it
    /// changes when any dependency of the plugin changes.
    #[serde_as(as = "SerializableHash::<rattler_digest::Sha256>")]
    pub plugin_sha256: Sha256Hash,

    /// The virtual package itself, as handed to the solver.
    pub package: GenericVirtualPackage,
}
