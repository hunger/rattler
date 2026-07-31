//! Running a plugin's entry point out of the environment it was installed in.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use rattler_conda_types::Platform;
use rattler_shell::activation::prefix_path_entries;
use tokio::io::AsyncReadExt;

/// How long a plugin may run before it is killed.
///
/// Detection happens on the way into a solve, and a plugin is meant to read a
/// version file or query a driver, not to work. A plugin that needs longer
/// than this would stall every solve that runs it.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(1);

/// The longest line a well-behaved plugin can need to write.
///
/// A verdict line cannot get long: a conda package's name, version and build
/// string together fit in an archive file name, which caps the three fields at
/// under 250 bytes before the ~60 bytes of JSON around them. What can get long
/// is a `cache` line watching a filesystem path, at most `PATH_MAX` (4096
/// bytes on Linux); twice that fits one maximal path even with every byte
/// JSON-escaped.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// The most output a plugin registered for `declared_count` virtual packages
/// may produce, counted across stdout and stderr together.
///
/// One line per registered virtual package, one `cache` line, and one line of
/// slack. Legitimate output is nowhere near this; without a bound, a
/// misbehaving plugin gets handed the client's memory.
pub fn output_budget(declared_count: usize) -> usize {
    MAX_LINE_BYTES.saturating_mul(declared_count.saturating_add(2))
}

/// What a plugin run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRun {
    /// Everything the plugin wrote to stdout, to be handed to
    /// [`parse_output`](crate::parse_output).
    pub stdout: String,

    /// Everything the plugin wrote to stderr. Diagnostics, for logging.
    pub stderr: String,

    /// The process exit code, or `None` if a signal killed it.
    ///
    /// Anything but `Some(0)` means the run failed and every virtual package the
    /// plugin was registered for has to be treated as absent.
    pub exit_code: Option<i32>,
}

impl PluginRun {
    /// Whether the plugin ran to completion, making its output authoritative.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// A plugin could not be run at all, as distinct from one that ran and failed.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// No executable named after the plugin package exists in the environment.
    #[error("'{entry_point}' is not in the plugin environment at '{}'", prefix.display())]
    EntryPointMissing {
        /// The executable that was looked for.
        entry_point: String,
        /// The prefix that was searched.
        prefix: PathBuf,
    },

    /// The executable exists but could not be started.
    #[error("failed to run '{}'", executable.display())]
    Spawn {
        /// The executable that could not be started.
        executable: PathBuf,
        /// Why it could not be started.
        #[source]
        source: std::io::Error,
    },

    /// The plugin was still running when [`RUN_TIMEOUT`] elapsed.
    #[error("'{}' was still running after {:?} and was killed", executable.display(), RUN_TIMEOUT)]
    TimedOut {
        /// The executable that was killed.
        executable: PathBuf,
    },

    /// The plugin wrote more than [`output_budget`] allows for its
    /// registration.
    #[error("'{}' produced more than {budget} bytes of output and was killed", executable.display())]
    TooMuchOutput {
        /// The executable that was killed.
        executable: PathBuf,
        /// The budget it exceeded, in bytes.
        budget: usize,
    },

    /// The plugin's output could not be read.
    #[error("failed to read the output of '{}'", executable.display())]
    Read {
        /// The executable whose output was being read.
        executable: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// Runs `entry_point` from `prefix` and collects what it said.
///
/// The executable is invoked directly rather than through a shell, with the
/// environment's binary directories prepended to `PATH` and `CONDA_PREFIX` set.
/// A shell would run the environment's `activate.d` scripts, and anything those
/// print lands on the same stdout the protocol is parsed from -- so a chatty
/// activation script would corrupt a plugin's output. Conda packages resolve
/// their own libraries through `RPATH`, so the cost of skipping activation is
/// limited to plugins that depend on `activate.d` side effects.
///
/// The run is bounded: a plugin still running when [`RUN_TIMEOUT`] elapses, or
/// one producing more than [`output_budget`]`(declared_count)` bytes of
/// output, is killed and reported as an error of its own. A non-zero exit is
/// reported in [`PluginRun::exit_code`] rather than as an error: the plugin
/// ran, it just failed, and the caller decides what that means.
pub async fn run_plugin(
    prefix: &Path,
    entry_point: &str,
    platform: Platform,
    declared_count: usize,
) -> Result<PluginRun, RunnerError> {
    let executable = find_entry_point(prefix, entry_point, platform).ok_or_else(|| {
        RunnerError::EntryPointMissing {
            entry_point: entry_point.to_string(),
            prefix: prefix.to_path_buf(),
        }
    })?;

    let mut child = tokio::process::Command::new(&executable)
        .env("PATH", prefixed_path(prefix, platform))
        .env("CONDA_PREFIX", prefix)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| RunnerError::Spawn {
            executable: executable.clone(),
            source,
        })?;

    let budget = output_budget(declared_count);
    match tokio::time::timeout(RUN_TIMEOUT, collect_output(&mut child, budget)).await {
        Ok(Ok(Collected::Complete(run))) => Ok(run),
        Ok(Ok(Collected::OverBudget)) => {
            kill(&mut child).await;
            Err(RunnerError::TooMuchOutput { executable, budget })
        }
        Ok(Err(source)) => {
            kill(&mut child).await;
            Err(RunnerError::Read { executable, source })
        }
        Err(_elapsed) => {
            kill(&mut child).await;
            Err(RunnerError::TimedOut { executable })
        }
    }
}

/// What reading a plugin's output until EOF ended in.
enum Collected {
    /// The plugin finished within its budget.
    Complete(PluginRun),
    /// The plugin exceeded its budget and must be killed.
    OverBudget,
}

/// Reads stdout and stderr to their ends, then waits for the exit status.
///
/// The two streams are drained together: reading one to its end first would
/// deadlock against a plugin blocked on writing the other. Every byte counts
/// against `budget`, and exceeding it stops the reading immediately -- the
/// point of the budget is not to buffer what a misbehaving plugin writes.
async fn collect_output(
    child: &mut tokio::process::Child,
    budget: usize,
) -> std::io::Result<Collected> {
    let mut stdout = child.stdout.take().expect("stdout was configured as piped");
    let mut stderr = child.stderr.take().expect("stderr was configured as piped");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_chunk = [0u8; 4096];
    let mut stderr_chunk = [0u8; 4096];
    let mut stdout_open = true;
    let mut stderr_open = true;

    while stdout_open || stderr_open {
        tokio::select! {
            read = stdout.read(&mut stdout_chunk), if stdout_open => match read? {
                0 => stdout_open = false,
                n => stdout_bytes.extend_from_slice(&stdout_chunk[..n]),
            },
            read = stderr.read(&mut stderr_chunk), if stderr_open => match read? {
                0 => stderr_open = false,
                n => stderr_bytes.extend_from_slice(&stderr_chunk[..n]),
            },
        }
        if stdout_bytes.len() + stderr_bytes.len() > budget {
            return Ok(Collected::OverBudget);
        }
    }

    let status = child.wait().await?;
    Ok(Collected::Complete(PluginRun {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code: status.code(),
    }))
}

/// Kills the plugin process on the way to reporting an error.
///
/// A kill can only fail when the process is already gone, so the error being
/// reported alongside stays the interesting one.
async fn kill(child: &mut tokio::process::Child) {
    if let Err(error) = child.kill().await {
        tracing::debug!("failed to kill the plugin process: {error}");
    }
}

/// Locates the executable named after the plugin package, trying the
/// extensions Windows needs to consider one runnable.
fn find_entry_point(prefix: &Path, entry_point: &str, platform: Platform) -> Option<PathBuf> {
    let extensions: &[&str] = if platform.is_windows() {
        &["", ".exe", ".bat", ".cmd"]
    } else {
        &[""]
    };

    prefix_path_entries(prefix, &platform)
        .into_iter()
        .flat_map(|dir| {
            extensions
                .iter()
                .map(move |extension| dir.join(format!("{entry_point}{extension}")))
        })
        .find(|candidate| candidate.is_file())
}

/// The environment's binary directories ahead of the inherited `PATH`, so a
/// plugin finds its own helpers before anything on the host.
fn prefixed_path(prefix: &Path, platform: Platform) -> OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let entries = prefix_path_entries(prefix, &platform)
        .into_iter()
        .chain(std::env::split_paths(&inherited));
    std::env::join_paths(entries).unwrap_or(inherited)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes an executable named `entry_point` into `prefix` running the
    /// platform's variant of the given script body.
    fn write_plugin_script(prefix: &Path, entry_point: &str, unix_body: &str, windows_body: &str) {
        let platform = Platform::current();
        let bin_dir = prefix_path_entries(prefix, &platform)
            .into_iter()
            .next()
            .expect("a platform always has at least one binary directory");
        std::fs::create_dir_all(&bin_dir).unwrap();

        if platform.is_windows() {
            std::fs::write(
                bin_dir.join(format!("{entry_point}.bat")),
                format!("@echo off\r\n{windows_body}"),
            )
            .unwrap();
        } else {
            let path = bin_dir.join(entry_point);
            std::fs::write(&path, format!("#!/bin/sh\n{unix_body}")).unwrap();

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }

    /// Writes an executable named `entry_point` into `prefix` that prints
    /// `stdout_lines` and exits with `exit_code`.
    fn write_fake_plugin(prefix: &Path, entry_point: &str, stdout_lines: &[&str], exit_code: i32) {
        let mut unix = String::new();
        let mut windows = String::new();
        for line in stdout_lines {
            unix.push_str(&format!("printf '%s\\n' '{line}'\n"));
            windows.push_str(&format!("echo {line}\r\n"));
        }
        unix.push_str(&format!("exit {exit_code}\n"));
        windows.push_str(&format!("exit /b {exit_code}\r\n"));
        write_plugin_script(prefix, entry_point, &unix, &windows);
    }

    #[tokio::test]
    async fn captures_stdout_of_a_successful_run() {
        let prefix = tempfile::tempdir().unwrap();
        write_fake_plugin(
            prefix.path(),
            "cuda-detect",
            &[
                r#"{"kind": "present", "name": "__cuda", "version": "12.4"}"#,
                r#"{"kind": "absent", "name": "__cuda_arch"}"#,
            ],
            0,
        );

        let run = run_plugin(prefix.path(), "cuda-detect", Platform::current(), 2)
            .await
            .unwrap();

        assert!(run.succeeded());
        // Round-trips through the protocol, which is the point of capturing it.
        let output = crate::parse_output(&run.stdout).unwrap();
        assert_eq!(output.detections.len(), 2);
    }

    /// A plugin that fails is not a runner error: it ran, and the caller decides
    /// what a failure means.
    #[tokio::test]
    async fn a_failing_plugin_is_reported_not_raised() {
        let prefix = tempfile::tempdir().unwrap();
        write_fake_plugin(prefix.path(), "broken-detect", &["not json"], 3);

        let run = run_plugin(prefix.path(), "broken-detect", Platform::current(), 1)
            .await
            .unwrap();

        assert!(!run.succeeded());
        assert_eq!(run.exit_code, Some(3));
        assert!(run.stdout.contains("not json"));
    }

    #[tokio::test]
    async fn a_missing_entry_point_is_an_error() {
        let prefix = tempfile::tempdir().unwrap();
        write_fake_plugin(prefix.path(), "cuda-detect", &[], 0);

        let err = run_plugin(prefix.path(), "rocm-detect", Platform::current(), 1)
            .await
            .unwrap_err();

        assert!(
            matches!(err, RunnerError::EntryPointMissing { .. }),
            "{err}"
        );
    }

    /// A plugin that hangs is killed after [`RUN_TIMEOUT`] instead of stalling
    /// detection -- and the solve waiting on it -- forever.
    #[tokio::test]
    async fn a_hanging_plugin_is_killed() {
        let prefix = tempfile::tempdir().unwrap();
        write_plugin_script(
            prefix.path(),
            "hang-detect",
            "sleep 5\n",
            "ping -n 6 127.0.0.1 >nul\r\n",
        );

        let err = run_plugin(prefix.path(), "hang-detect", Platform::current(), 1)
            .await
            .unwrap_err();

        assert!(matches!(err, RunnerError::TimedOut { .. }), "{err}");
    }

    /// A plugin that writes far more than its registration could need is
    /// killed once it exceeds its budget, before it can exhaust memory.
    #[tokio::test]
    async fn a_plugin_exceeding_its_output_budget_is_killed() {
        let prefix = tempfile::tempdir().unwrap();
        let line = "x".repeat(1024);
        // Three times the budget for a plugin registered for nothing.
        let lines = vec![line.as_str(); 3 * output_budget(0) / 1024];
        write_fake_plugin(prefix.path(), "spew-detect", &lines, 0);

        let err = run_plugin(prefix.path(), "spew-detect", Platform::current(), 0)
            .await
            .unwrap_err();

        assert!(matches!(err, RunnerError::TooMuchOutput { .. }), "{err}");
    }

    /// The budget grows with the registration: one line per declared virtual
    /// package, one cache line, one line of slack.
    #[test]
    fn the_budget_scales_with_the_registration() {
        assert_eq!(output_budget(0), 2 * MAX_LINE_BYTES);
        assert_eq!(output_budget(3), 5 * MAX_LINE_BYTES);
    }

    /// The plugin's own binary directory has to come first, or a plugin calling
    /// a helper would get the host's copy.
    #[test]
    fn the_environment_comes_first_on_path() {
        let prefix = Path::new("/tmp/does-not-need-to-exist");
        let path = prefixed_path(prefix, Platform::current());
        let first = std::env::split_paths(&path).next().unwrap();
        assert_eq!(
            first,
            prefix_path_entries(prefix, &Platform::current())[0],
            "the environment must precede the inherited PATH"
        );
    }
}
