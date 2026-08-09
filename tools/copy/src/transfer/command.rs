//! Small process-command adapter shared by transfer-side cleanup and telemetry.

use crate::domain::CmdOutput;
use std::io;
use std::process::Command;

pub(crate) fn run_command_capture(cmd: &[String], sudo: bool) -> io::Result<CmdOutput> {
    let mut full = Vec::with_capacity(cmd.len() + usize::from(sudo));
    if sudo {
        full.push("pkexec".to_string());
    }
    full.extend(cmd.iter().cloned());

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
