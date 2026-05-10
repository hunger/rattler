//! Utility functions for working with the terminal output

/// Log the output of a command to terminal
pub fn log_output(cmd_info: &str, output: std::process::Output) {
    tracing::info!("{}: status {}", cmd_info, output.status);
    tracing::info!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_family = "unix")]
pub fn run_pre_create_command(pre_create_command: &str) -> Result<(), crate::MenuInstError> {
    use std::os::unix::fs::PermissionsExt;
    use std::{io::Write, process::Command};

    use crate::MenuInstError;

    let mut temp_file = tempfile::NamedTempFile::with_suffix(".sh")?;
    temp_file.write_all(pre_create_command.as_bytes())?;

    let runs_directly = pre_create_command.starts_with("!#");
    if runs_directly {
        // `fchmod` on the still-open file descriptor -- no path
        // resolution, no race window. Doing the chmod through
        // `set_permissions(&path, ...)` after `into_temp_path()`
        // would let a co-tenant race a symlink swap of the
        // tempfile path under `$TMPDIR`.
        temp_file
            .as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }

    let temp_path = temp_file.into_temp_path();

    // Mark the file as executable or run it with bash
    let mut cmd = if runs_directly {
        Command::new(&temp_path)
    } else {
        let mut cmd = Command::new("bash");
        cmd.arg(&temp_path);
        cmd
    };

    let output = cmd.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(MenuInstError::InstallError(format!(
            "Failed to run pre-create command (status: {}): \nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}
