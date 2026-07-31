//! Checking a plugin against what its channel registered it for.
//!
//! The registration in `info.virtual_package_plugins` is a promise: this plugin
//! speaks for exactly these virtual packages. Enforcing it before anything
//! reaches the solver keeps a plugin from quietly claiming names its channel
//! never advertised, which is checkable without trusting the plugin.

use std::collections::BTreeSet;

use rattler_conda_types::PackageName;

use crate::protocol::PluginOutput;

/// A plugin's output does not match what its channel registered it for.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContractViolation {
    /// The plugin reported a virtual package it was not registered for.
    #[error(
        "the plugin reported {} which its channel did not register it for",
        format_names(undeclared)
    )]
    Undeclared {
        /// The names that were not registered, sorted.
        undeclared: Vec<PackageName>,
    },

    /// The plugin gave no verdict for something it was registered for. Absence
    /// is reported with an explicit null version, so silence is a bug in the
    /// plugin rather than a system without that hardware.
    #[error(
        "the plugin gave no verdict for {}, which its channel registered it for",
        format_names(missing)
    )]
    Missing {
        /// The names that were registered but not reported, sorted.
        missing: Vec<PackageName>,
    },

    /// The plugin reported the same virtual package more than once.
    #[error("the plugin reported {} more than once", format_names(duplicated))]
    Duplicated {
        /// The names reported more than once, sorted.
        duplicated: Vec<PackageName>,
    },
}

fn format_names(names: &[PackageName]) -> String {
    names
        .iter()
        .map(PackageName::as_source)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Check that a plugin gave exactly one verdict for every virtual package its
/// channel registered it for, and none for anything else.
///
/// Duplicates are reported first, since a duplicate makes the other two
/// answers ambiguous.
pub fn validate(
    declared: &BTreeSet<PackageName>,
    output: &PluginOutput,
) -> Result<(), ContractViolation> {
    let mut seen = BTreeSet::new();
    let mut duplicated = BTreeSet::new();
    for detection in &output.detections {
        if !seen.insert(detection.name.clone()) {
            duplicated.insert(detection.name.clone());
        }
    }
    if !duplicated.is_empty() {
        return Err(ContractViolation::Duplicated {
            duplicated: duplicated.into_iter().collect(),
        });
    }

    let undeclared: Vec<_> = seen.difference(declared).cloned().collect();
    if !undeclared.is_empty() {
        return Err(ContractViolation::Undeclared { undeclared });
    }

    let missing: Vec<_> = declared.difference(&seen).cloned().collect();
    if !missing.is_empty() {
        return Err(ContractViolation::Missing { missing });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_output;

    fn declared(names: &[&str]) -> BTreeSet<PackageName> {
        names
            .iter()
            .map(|n| PackageName::new_unchecked(*n))
            .collect()
    }

    fn output(lines: &[&str]) -> PluginOutput {
        parse_output(&lines.join("\n")).expect("valid protocol")
    }

    fn present(name: &str) -> String {
        format!(r#"{{"kind": "present", "name": "{name}", "version": "1"}}"#)
    }

    fn absent(name: &str) -> String {
        format!(r#"{{"kind": "absent", "name": "{name}"}}"#)
    }

    #[test]
    fn exact_coverage_passes() {
        let out = output(&[&present("__cuda"), &present("__cuda_arch")]);
        assert_eq!(
            validate(&declared(&["__cuda", "__cuda_arch"]), &out),
            Ok(())
        );
    }

    /// A machine without the hardware is the common case: every registered name
    /// still gets a verdict, they are just all absent.
    #[test]
    fn all_absent_passes() {
        let out = output(&[&absent("__cuda"), &absent("__cuda_arch")]);
        assert_eq!(
            validate(&declared(&["__cuda", "__cuda_arch"]), &out),
            Ok(())
        );
    }

    #[test]
    fn undeclared_name_is_rejected() {
        let out = output(&[&present("__cuda"), &present("__rocm")]);
        assert_eq!(
            validate(&declared(&["__cuda"]), &out),
            Err(ContractViolation::Undeclared {
                undeclared: vec![PackageName::new_unchecked("__rocm")]
            })
        );
    }

    #[test]
    fn silence_about_a_registered_name_is_rejected() {
        let out = output(&[&present("__cuda")]);
        assert_eq!(
            validate(&declared(&["__cuda", "__cuda_arch"]), &out),
            Err(ContractViolation::Missing {
                missing: vec![PackageName::new_unchecked("__cuda_arch")]
            })
        );
    }

    #[test]
    fn duplicate_verdict_is_rejected_before_anything_else() {
        // Also undeclared, but the duplicate is what gets reported.
        let out = output(&[&present("__rocm"), &absent("__rocm")]);
        assert_eq!(
            validate(&declared(&["__cuda"]), &out),
            Err(ContractViolation::Duplicated {
                duplicated: vec![PackageName::new_unchecked("__rocm")]
            })
        );
    }

    #[test]
    fn registering_nothing_permits_nothing() {
        assert_eq!(validate(&declared(&[]), &output(&[])), Ok(()));
        assert!(validate(&declared(&[]), &output(&[&present("__cuda")])).is_err());
    }

    #[test]
    fn violations_name_every_offender() {
        let out = output(&[&present("__a"), &present("__b")]);
        let err = validate(&declared(&["__c"]), &out).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the plugin reported __a, __b which its channel did not register it for"
        );
    }
}
