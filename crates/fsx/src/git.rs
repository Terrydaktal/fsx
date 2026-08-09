use crate::path::full_path;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusPair {
    pub staged: char,
    pub worktree: char,
}

pub fn parse_status_pair(status: &str) -> StatusPair {
    if status == "!!" {
        return StatusPair {
            staged: ' ',
            worktree: '!',
        };
    }
    if status == "??" {
        return StatusPair {
            staged: ' ',
            worktree: '?',
        };
    }
    let mut chars = status.chars();
    StatusPair {
        staged: chars.next().unwrap_or(' '),
        worktree: chars.next().unwrap_or(' '),
    }
}

pub fn display_status_symbol(status: char) -> char {
    match status {
        ' ' => '-',
        '?' => 'N',
        '!' => 'I',
        other => other,
    }
}

pub fn normalize_status(status: &str) -> char {
    if status == "!!" {
        'I'
    } else {
        display_status_symbol(parse_status_pair(status).staged)
    }
}

pub fn parse_porcelain_v1_z(raw: &[u8]) -> HashMap<PathBuf, String> {
    parse_porcelain_v1_z_at(raw, Path::new("."))
}

pub fn parse_porcelain_v1_z_at(raw: &[u8], root: &Path) -> HashMap<PathBuf, String> {
    let mut statuses = HashMap::new();
    let mut fields = raw.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 3 {
            continue;
        }
        let status = String::from_utf8_lossy(&field[..2]).to_string();
        let first_path = &field[3..];
        let path = path_from_bytes(first_path);
        let path = full_path(&root.join(path));
        statuses.insert(path, status.clone());
        if (status.starts_with('R') || status.starts_with('C'))
            && let Some(previous) = fields.next()
        {
            statuses.insert(full_path(&root.join(path_from_bytes(previous))), status);
        }
    }
    statuses
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

pub fn status_map(root: &Path) -> io::Result<HashMap<PathBuf, String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=matching",
            "--untracked-files=all",
        ])
        .output()?;
    if !output.status.success() {
        return Ok(HashMap::new());
    }
    Ok(parse_porcelain_v1_z_at(&output.stdout, root))
}
