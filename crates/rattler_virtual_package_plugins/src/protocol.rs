//! The wire format a detection plugin writes to stdout.
//!
//! One JSON object per line, so a plugin can emit results as it discovers them
//! and a malformed line can be reported with its line number instead of
//! invalidating the whole run.
//!
//! ```text
//! {"kind": "present", "name": "__cuda", "version": "12.4"}
//! {"kind": "present", "name": "__cuda_arch", "version": "0", "build_string": "sm_89"}
//! {"kind": "absent", "name": "__rocm"}
//! {"kind": "cache", "ttl_seconds": 86400, "watch_paths": ["/sys/module/amdgpu/version"]}
//! ```
//!
//! `absent` is a line kind of its own rather than a null version. A plugin has
//! to give a verdict on every virtual package its channel registered it for, so
//! "not on this system" must be something it can say out loud; silence is a
//! contract violation instead.

use rattler_conda_types::{GenericVirtualPackage, PackageName, Version};
use serde::{Deserialize, Serialize};

/// A single line of plugin output.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginLine {
    /// The virtual package is on this system, at this version.
    Present {
        /// The virtual package this verdict is about.
        name: PackageName,
        /// The detected version.
        version: Version,
        /// The build string, for virtual packages that carry their information
        /// there rather than in the version, such as `__archspec`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build_string: Option<String>,
    },

    /// The virtual package is not on this system.
    Absent {
        /// The virtual package this verdict is about.
        name: PackageName,
    },

    /// How long this run's verdicts may be reused. At most one per run.
    Cache(CachePolicy),
}

/// A plugin's verdict on one virtual package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The virtual package this verdict is about.
    pub name: PackageName,

    /// What was detected, or `None` when the plugin reported it absent.
    pub detected: Option<Detected>,
}

impl Verdict {
    /// The solver-facing form, or `None` when the plugin reported the virtual
    /// package absent.
    pub fn to_generic(&self) -> Option<GenericVirtualPackage> {
        let detected = self.detected.as_ref()?;
        Some(GenericVirtualPackage {
            name: self.name.clone(),
            version: detected.version.clone(),
            build_string: detected.build_string.clone().unwrap_or_default(),
        })
    }
}

/// What a plugin found for a virtual package that is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// The detected version.
    pub version: Version,

    /// The build string, if the plugin reported one.
    pub build_string: Option<String>,
}

/// How long a set of verdicts may be reused before the plugin must run again.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct CachePolicy {
    /// Seconds the verdicts stay valid. `None` means no time limit, so only
    /// `watch_paths` can invalidate them.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,

    /// Paths whose existence or modification time invalidates the verdicts.
    /// This is what catches a driver upgrade between two solves.
    #[serde(default)]
    pub watch_paths: Vec<String>,
}

/// Everything one plugin run reported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginOutput {
    /// One verdict per virtual package, in the order the plugin emitted them.
    pub detections: Vec<Verdict>,

    /// The plugin's cache policy, if it declared one.
    pub cache_policy: Option<CachePolicy>,
}

/// A plugin wrote something that is not valid output.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// A line was not a valid protocol object.
    #[error("line {line}: {source}")]
    Malformed {
        /// 1-based line number, so it matches what a user sees in a log.
        line: usize,
        /// The underlying parse failure.
        source: serde_json::Error,
    },

    /// More than one cache policy was emitted; which one applies is undefined.
    #[error("line {line}: a second cache policy was reported")]
    DuplicateCachePolicy {
        /// 1-based line number of the offending policy.
        line: usize,
    },
}

/// Parse one line of plugin output.
pub fn parse_line(line: &str) -> Result<PluginLine, serde_json::Error> {
    serde_json::from_str(line)
}

/// Parse a plugin's entire stdout.
///
/// Blank lines are ignored so trailing newlines and shell-script padding do not
/// matter. Every other line must parse.
pub fn parse_output(stdout: &str) -> Result<PluginOutput, ProtocolError> {
    let mut output = PluginOutput::default();

    for (index, line) in stdout.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }

        match parse_line(line).map_err(|source| ProtocolError::Malformed {
            line: line_number,
            source,
        })? {
            PluginLine::Present {
                name,
                version,
                build_string,
            } => output.detections.push(Verdict {
                name,
                detected: Some(Detected {
                    version,
                    build_string,
                }),
            }),
            PluginLine::Absent { name } => output.detections.push(Verdict {
                name,
                detected: None,
            }),
            PluginLine::Cache(policy) => {
                if output.cache_policy.is_some() {
                    return Err(ProtocolError::DuplicateCachePolicy { line: line_number });
                }
                output.cache_policy = Some(policy);
            }
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(output: &PluginOutput, name: &str) -> Verdict {
        output
            .detections
            .iter()
            .find(|v| v.name.as_source() == name)
            .expect("verdict present")
            .clone()
    }

    #[test]
    fn parses_present_absent_and_policy() {
        let stdout = r#"
{"kind": "present", "name": "__cuda", "version": "12.4"}
{"kind": "present", "name": "__cuda_arch", "version": "0", "build_string": "sm_89"}
{"kind": "absent", "name": "__rocm"}
{"kind": "cache", "ttl_seconds": 86400, "watch_paths": ["/sys/module/amdgpu/version"]}
"#;
        let output = parse_output(stdout).unwrap();

        assert_eq!(output.detections.len(), 3);
        assert_eq!(
            verdict(&output, "__cuda").to_generic().unwrap().to_string(),
            "__cuda=12.4"
        );
        assert_eq!(
            verdict(&output, "__cuda_arch")
                .to_generic()
                .unwrap()
                .to_string(),
            "__cuda_arch=0=sm_89"
        );
        assert!(
            verdict(&output, "__rocm").to_generic().is_none(),
            "an absent line yields no virtual package"
        );

        let policy = output.cache_policy.unwrap();
        assert_eq!(policy.ttl_seconds, Some(86400));
        assert_eq!(policy.watch_paths, ["/sys/module/amdgpu/version"]);
    }

    #[test]
    fn empty_output_is_valid_and_carries_no_policy() {
        let output = parse_output("\n\n   \n").unwrap();
        assert!(output.detections.is_empty());
        assert!(output.cache_policy.is_none());
    }

    /// A `present` line has to say which version and an `absent` line has to not
    /// carry one. Reporting absence by omitting the version would give silence
    /// and absence the same wire shape, which the contract has to tell apart.
    #[test]
    fn present_requires_a_version_and_absent_refuses_one() {
        for line in [
            r#"{"kind": "present", "name": "__cuda"}"#,
            r#"{"kind": "absent", "name": "__cuda", "version": "12.4"}"#,
            r#"{"kind": "present", "name": "__cuda", "version": null}"#,
        ] {
            assert!(parse_output(line).is_err(), "should reject: {line}");
        }
    }

    #[test]
    fn reports_the_offending_line_number() {
        let stdout = concat!(
            "{\"kind\": \"present\", \"name\": \"__cuda\", \"version\": \"12.4\"}\n",
            "\n",
            "not json\n",
        );
        let err = parse_output(stdout).unwrap_err();
        // Blank lines are skipped but still counted, so the number matches a log.
        assert!(
            matches!(err, ProtocolError::Malformed { line: 3, .. }),
            "{err}"
        );
    }

    #[test]
    fn unknown_fields_and_kinds_are_rejected() {
        for line in [
            r#"{"kind": "present", "name": "__cuda", "version": "1", "extra": true}"#,
            r#"{"kind": "something_else", "name": "__cuda"}"#,
            r#"{"name": "__cuda", "version": "1"}"#,
        ] {
            assert!(parse_output(line).is_err(), "should reject: {line}");
        }
    }

    #[test]
    fn a_second_cache_policy_is_rejected() {
        let stdout = concat!(
            "{\"kind\": \"cache\", \"ttl_seconds\": 1}\n",
            "{\"kind\": \"cache\", \"ttl_seconds\": 2}\n",
        );
        let err = parse_output(stdout).unwrap_err();
        assert!(
            matches!(err, ProtocolError::DuplicateCachePolicy { line: 2 }),
            "{err}"
        );
    }
}
