//! Producing a set of virtual packages, from whatever source provides them.
//!
//! A caller assembling the virtual packages for a solve has two kinds of source
//! to deal with: the ones this client detects itself, which CEP 30 obliges it to
//! offer, and the ones a channel's plugin reports. They behave differently --
//! one is a synchronous read of the running system, the other installs an
//! environment and starts a process -- but a caller should not have to care
//! which it is holding.
//!
//! [`VirtualPackageFactory`] is that common shape. It separates the cheap
//! question from the expensive one:
//!
//! - [`provides`](VirtualPackageFactory::provides) is the set of names this
//!   source speaks for. It costs nothing: no detection, no plugin run.
//! - [`resolve`](VirtualPackageFactory::resolve) is what is actually on this
//!   system, and may be slow.
//!
//! That split is the point of the abstraction. A caller can see what a factory
//! *would* answer for and skip resolving one whose names nothing needs, rather
//! than paying for every plugin a channel happens to register. In both
//! specializations `provides` is what the source claims and `resolve` is what
//! turned out to be there: names reported absent simply do not come back.

use std::collections::BTreeSet;

use async_trait::async_trait;
use rattler_conda_types::{PackageName, SourcedVirtualPackage, VirtualPackageSource};
use rattler_virtual_packages::{
    DetectVirtualPackageError, VirtualPackage, VirtualPackageOverrides,
};

/// A source of virtual packages.
#[async_trait]
pub trait VirtualPackageFactory {
    /// The virtual packages this source speaks for.
    ///
    /// Cheap by contract: a caller uses this to decide whether resolving is
    /// worth it, so an implementation must not detect anything here.
    fn provides(&self) -> &BTreeSet<PackageName>;

    /// What this source finds on the running system.
    ///
    /// Only names in [`provides`](Self::provides) can come back, and fewer of
    /// them: a name this source speaks for but does not find is absent rather
    /// than reported.
    async fn resolve(&self) -> Result<Vec<SourcedVirtualPackage>, FactoryError>;
}

/// A factory could not produce its virtual packages.
#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    /// This system's own virtual packages could not be determined.
    #[error("failed to determine the virtual packages of this system")]
    BuiltIn(#[from] DetectVirtualPackageError),
}

/// The virtual packages this client detects itself.
///
/// CEP 30 makes these an obligation of the client rather than of any channel, so
/// this factory is present in every view and its results carry no channel. It is
/// also the weakest source: a plugin claiming one of these names overrides it,
/// because the CEP requires the name to be *present* and does not dictate that
/// the client's own detection is what fills it.
pub struct BuiltinVirtualPackages {
    provides: BTreeSet<PackageName>,
    overrides: VirtualPackageOverrides,
}

impl BuiltinVirtualPackages {
    /// Detects with the `CONDA_OVERRIDE_*` variables this process was started
    /// with, which is what CEP 30 specifies for them.
    pub fn from_env() -> Self {
        Self::with_overrides(VirtualPackageOverrides::from_env())
    }

    /// Detects with the given overrides.
    pub fn with_overrides(overrides: VirtualPackageOverrides) -> Self {
        Self {
            provides: STANDARDIZED_VIRTUAL_PACKAGES
                .iter()
                .map(|name| PackageName::new_unchecked(*name))
                .collect(),
            overrides,
        }
    }
}

/// Every virtual package this client knows how to look for.
///
/// Written out rather than derived because [`rattler_virtual_packages`] exposes
/// no enumeration: the names live in the `From<VirtualPackage> for
/// GenericVirtualPackage` impls. `standardized_names_stay_in_sync` guards the
/// drift, but only for names some platform detects by default -- ones that never
/// are (`__cuda`, `__cuda_arch`, and the non-glibc libc flavours) have to be
/// added here by hand.
///
/// A fixed list rather than the result of detecting is what keeps
/// [`provides`](VirtualPackageFactory::provides) honest about costing nothing.
/// It claims names this machine may turn out not to have, which is exactly what
/// `provides` means: `__cuda` is a name this client speaks for even where there
/// is no GPU to find.
pub const STANDARDIZED_VIRTUAL_PACKAGES: &[&str] = &[
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

#[async_trait]
impl VirtualPackageFactory for BuiltinVirtualPackages {
    fn provides(&self) -> &BTreeSet<PackageName> {
        &self.provides
    }

    async fn resolve(&self) -> Result<Vec<SourcedVirtualPackage>, FactoryError> {
        Ok(VirtualPackage::detect(&self.overrides)?
            .into_iter()
            .map(|package| SourcedVirtualPackage {
                source: VirtualPackageSource::BuiltIn,
                package: package.into(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use rattler_conda_types::Platform;
    use rattler_virtual_packages::VirtualPackages;

    use super::*;

    /// CEP 30 requires `__archspec` of every client on every platform, so it is
    /// the one name that must be there whatever the machine is.
    #[tokio::test]
    async fn the_built_ins_include_what_cep_30_always_requires() {
        let factory = BuiltinVirtualPackages::from_env();

        let archspec = PackageName::new_unchecked("__archspec");
        assert!(
            factory.provides().contains(&archspec),
            "CEP 30 requires __archspec to always be present, got {:?}",
            factory.provides()
        );

        let resolved = factory.resolve().await.unwrap();
        assert!(
            resolved
                .iter()
                .any(|detected| detected.package.name == archspec)
        );
    }

    /// Built-ins belong to no channel: they come from the client, and are
    /// visible in every view whatever the channels are.
    #[tokio::test]
    async fn built_ins_carry_no_channel() {
        let factory = BuiltinVirtualPackages::from_env();

        for detected in factory.resolve().await.unwrap() {
            assert!(
                detected.source.is_built_in(),
                "{:?} should be a built-in",
                detected.package.name
            );
            assert_eq!(detected.source.channel(), None);
        }
    }

    /// Nothing is resolved that was not promised, which is what lets a caller
    /// skip a factory on the strength of `provides` alone.
    #[tokio::test]
    async fn resolving_answers_only_for_what_it_promised() {
        let factory = BuiltinVirtualPackages::from_env();

        for detected in factory.resolve().await.unwrap() {
            assert!(
                factory.provides().contains(&detected.package.name),
                "{:?} was resolved but never promised",
                detected.package.name
            );
        }
    }

    /// Asking what a factory speaks for must not detect. The built-in list is
    /// the same on a machine with a GPU and one without, which is what makes it
    /// safe for a caller to ask before deciding whether to pay for `resolve`.
    #[test]
    fn provides_does_not_depend_on_the_machine() {
        let names: Vec<_> = BuiltinVirtualPackages::from_env()
            .provides()
            .iter()
            .map(|name| name.as_source().to_string())
            .collect();

        let mut expected: Vec<_> = STANDARDIZED_VIRTUAL_PACKAGES
            .iter()
            .map(ToString::to_string)
            .collect();
        expected.sort();
        assert_eq!(names, expected);
    }

    /// The hand-written list has to keep up with what detection actually
    /// produces, or a plugin could claim a name this client also fills without
    /// anything noticing the clash.
    ///
    /// Only covers names that appear in per-platform detection, so ones that are
    /// never a default (`__cuda`, `__cuda_arch`, and the non-glibc libc
    /// flavours) still have to be added by hand.
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
}
