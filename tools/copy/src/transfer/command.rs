//! Small process-command adapter shared by transfer-side cleanup and telemetry.

use crate::domain::CmdOutput;
use std::ffi::OsString;
use std::io;
use std::process::Command;

pub(super) fn privileged_command(cmd: &[String], sudo: bool) -> Vec<String> {
    let mut full = Vec::with_capacity(cmd.len() + usize::from(sudo) * 2);
    if sudo {
        full.push("sudo".to_string());
        full.push("--".to_string());
    }
    full.extend(cmd.iter().cloned());
    full
}

pub(super) fn privileged_command_os(cmd: &[OsString], sudo: bool) -> Vec<OsString> {
    let mut full = Vec::with_capacity(cmd.len() + usize::from(sudo) * 2);
    if sudo {
        full.push(OsString::from("sudo"));
        full.push(OsString::from("--"));
    }
    full.extend(cmd.iter().cloned());
    full
}

pub(crate) fn run_command_capture(cmd: &[String], sudo: bool) -> io::Result<CmdOutput> {
    let full = privileged_command(cmd, sudo);
    let output = Command::new(&full[0]).args(&full[1..]).output()?;
    let code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if code != 0 {
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        if !detail.is_empty() {
            eprintln!("copy-rs: command failed ({code}): {detail}");
        }
    }
    Ok(CmdOutput { code })
}

pub(crate) fn run_command_capture_os(cmd: &[OsString], sudo: bool) -> io::Result<CmdOutput> {
    let full = privileged_command_os(cmd, sudo);
    let output = Command::new(&full[0]).args(&full[1..]).output()?;
    let code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if code != 0 {
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        if !detail.is_empty() {
            eprintln!("copy-rs: command failed ({code}): {detail}");
        }
    }
    Ok(CmdOutput { code })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_commands_use_sudo_end_of_options() {
        let command = ["true".to_string()];
        let full = privileged_command(&command, true);
        assert_eq!(full, ["sudo", "--", "true"]);
    }

    #[test]
    fn ordinary_commands_are_not_wrapped() {
        let command = ["true".to_string()];
        assert_eq!(privileged_command(&command, false), command);
    }
}
