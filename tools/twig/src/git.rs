use crate::fs_ops::to_full_path;
use crate::model::EntryInfo;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn populate_git_columns(
    listing_path: &Path,
    entries: &mut [EntryInfo],
) -> (bool, bool, bool) {
    let listing_abs = fs::canonicalize(listing_path).unwrap_or_else(|_| to_full_path(listing_path));
    let status_path = if fs::symlink_metadata(&listing_abs)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        listing_abs.clone()
    } else {
        listing_abs
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };

    let repo_root = git_repo_root(&status_path);
    let show_git_status = repo_root.is_some();
    if show_git_status {
        let status_map =
            collect_git_statuses_with_root(&status_path, repo_root.as_deref()).unwrap_or_default();
        for entry in entries.iter_mut() {
            let status = status_map
                .get(&entry.display_name)
                .copied()
                .unwrap_or(('-', '-'));
            entry.git_status = Some(status);
        }
    }

    let mut show_repo_status = false;
    let mut show_remote_status = false;
    let mut repo_marker_cache: HashMap<PathBuf, (Option<char>, Option<char>)> = HashMap::new();
    for entry in entries.iter_mut() {
        let is_dirish = if entry.is_symlink {
            entry.is_target_dir
        } else {
            entry.is_dir
        };
        if !is_dirish {
            continue;
        }
        let (repo_status, remote_status) = repo_marker_cache
            .entry(entry.actual_path.clone())
            .or_insert_with(|| git_repo_root_markers(&entry.actual_path))
            .to_owned();
        if repo_status.is_some() {
            show_repo_status = true;
        }
        entry.repo_status = repo_status;
        if remote_status.is_some() {
            show_remote_status = true;
        }
        entry.repo_remote_status = remote_status;
    }

    (show_git_status, show_repo_status, show_remote_status)
}

pub(crate) fn collect_git_statuses_with_root(
    listing_path: &Path,
    repo_root: Option<&Path>,
) -> Option<HashMap<String, (char, char)>> {
    let listing_abs = fs::canonicalize(listing_path).unwrap_or_else(|_| to_full_path(listing_path));
    let command_path = if fs::symlink_metadata(&listing_abs)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        listing_abs.clone()
    } else {
        listing_abs
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let repo_root = repo_root?;
    let rel_prefix = command_path
        .strip_prefix(&repo_root)
        .ok()
        .unwrap_or(Path::new(""));

    let output = Command::new("git")
        .arg("-C")
        .arg(&command_path)
        .arg("-c")
        .arg("core.quotepath=false")
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=matching",
            "--untracked-files=all",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let mut status_map: HashMap<String, (char, char)> = HashMap::new();
    let mut records = output.stdout.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.len() < 3 {
            continue;
        }

        let xy = std::str::from_utf8(&record[..2]).unwrap_or("  ");
        let path_part = &record[3..];

        let (staged_raw, unstaged_raw) = parse_git_status_pair(xy);

        let status_rel = Path::new(std::ffi::OsStr::from_bytes(path_part));
        let relevant = if rel_prefix.as_os_str().is_empty() {
            status_rel
        } else {
            match status_rel.strip_prefix(rel_prefix) {
                Ok(p) => p,
                Err(_) => continue,
            }
        };
        if relevant.as_os_str().is_empty() {
            continue;
        }

        let Some(top_component) = relevant.components().next() else {
            continue;
        };
        let top_name = top_component.as_os_str().to_string_lossy().into_owned();
        let staged = git_status_symbol(staged_raw);
        let unstaged = git_status_symbol(unstaged_raw);
        let current = status_map.entry(top_name).or_insert(('-', '-'));
        current.0 = pick_stronger_git_status(current.0, staged);
        current.1 = pick_stronger_git_status(current.1, unstaged);

        // Porcelain v1 -z emits the old path as a second NUL record for
        // renames/copies. It has no status prefix and must not be parsed as a
        // separate file.
        if matches!(staged_raw, 'R' | 'C') || matches!(unstaged_raw, 'R' | 'C') {
            let _ = records.next();
        }
    }

    Some(status_map)
}

pub(crate) fn git_repo_root(path: &Path) -> Option<PathBuf> {
    let command_path = if fs::symlink_metadata(path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(command_path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return None;
    }
    Some(PathBuf::from(root))
}

fn collect_git_repo_roots_in_directory(path: &Path) -> Vec<PathBuf> {
    let abs_path = fs::canonicalize(path).unwrap_or_else(|_| to_full_path(path));
    let mut roots: HashSet<PathBuf> = HashSet::new();

    // Include the enclosing repo when listing inside any subdirectory of a repo.
    if let Some(repo_root) = git_repo_root(&abs_path) {
        roots.insert(fs::canonicalize(&repo_root).unwrap_or(repo_root));
    }

    let entries = match fs::read_dir(&abs_path) {
        Ok(v) => v,
        Err(_) => return roots.into_iter().collect(),
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        if !path.join(".git").exists() {
            continue;
        }
        roots.insert(fs::canonicalize(&path).unwrap_or(path));
    }

    roots.into_iter().collect()
}

fn git_fetch_repo(repo_root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["fetch", "--all", "--prune", "--quiet"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) fn git_fetch_repos_for_listing_path(path: &Path) {
    let listing_dir = if fs::symlink_metadata(path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let mut repos = collect_git_repo_roots_in_directory(&listing_dir);
    if repos.is_empty() {
        return;
    }
    repos.sort();
    for repo in repos {
        let _ = git_fetch_repo(&repo);
    }
}

pub(crate) fn git_repo_root_markers(path: &Path) -> (Option<char>, Option<char>) {
    let abs_path = fs::canonicalize(path).unwrap_or_else(|_| to_full_path(path));
    if !abs_path.join(".git").exists() {
        return (None, None);
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(&abs_path)
        .args([
            "status",
            "--porcelain=2",
            "--branch",
            "--untracked-files=normal",
        ])
        .output();
    let Ok(output) = output else {
        return (Some('~'), Some('?'));
    };
    if !output.status.success() {
        return (Some('~'), Some('?'));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut has_upstream = false;
    let mut ahead = 0u64;
    let mut behind = 0u64;
    let mut has_ab = false;
    let mut dirty = false;

    for line in stdout.lines() {
        if line.starts_with("# ") {
            if let Some(rest) = line.strip_prefix("# branch.upstream ") {
                if !rest.trim().is_empty() {
                    has_upstream = true;
                }
            } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
                let mut parts = rest.split_whitespace();
                let ahead_part = parts.next().unwrap_or("");
                let behind_part = parts.next().unwrap_or("");
                if let Some(ahead_text) = ahead_part.strip_prefix('+') {
                    ahead = ahead_text.parse::<u64>().unwrap_or(0);
                }
                if let Some(behind_text) = behind_part.strip_prefix('-') {
                    behind = behind_text.parse::<u64>().unwrap_or(0);
                }
                has_ab = true;
            }
            continue;
        }
        if !line.trim().is_empty() {
            dirty = true;
        }
    }

    let repo_status = if dirty { '+' } else { '|' };
    let remote_status = if !has_upstream || !has_ab {
        '?'
    } else if ahead > 0 && behind > 0 {
        '↕'
    } else if ahead > 0 {
        '↑'
    } else if behind > 0 {
        '↓'
    } else {
        '✓'
    };

    (Some(repo_status), Some(remote_status))
}

fn git_status_symbol(raw: char) -> char {
    match raw {
        ' ' => '-',
        'M' => 'M',
        'A' => 'A',
        '?' => 'N',
        'D' => 'D',
        'R' => 'R',
        'T' => 'T',
        '!' => 'I',
        'U' => 'U',
        'C' => 'M',
        _ => '-',
    }
}

fn parse_git_status_pair(xy: &str) -> (char, char) {
    match xy {
        "??" => (' ', '?'),
        "!!" => (' ', '!'),
        // Any unmerged state should read as conflicted in both columns.
        "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU" => ('U', 'U'),
        _ => {
            let mut chars = xy.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            (x, y)
        }
    }
}

fn git_status_rank(symbol: char) -> u8 {
    match symbol {
        'U' => 8,
        'D' => 7,
        'R' => 6,
        'T' => 5,
        'M' => 4,
        'A' | 'N' => 3,
        'I' => 2,
        '-' => 1,
        _ => 0,
    }
}

fn pick_stronger_git_status(current: char, next: char) -> char {
    if git_status_rank(next) >= git_status_rank(current) {
        next
    } else {
        current
    }
}

pub(crate) fn git_symbol_style(symbol: char) -> nu_ansi_term::Style {
    match symbol {
        'N' | 'A' => nu_ansi_term::Color::Green.normal(),
        'M' | 'R' | 'T' => nu_ansi_term::Color::Yellow.normal(),
        'D' | 'U' => nu_ansi_term::Color::Red.normal(),
        '-' | 'I' => nu_ansi_term::Style::default().dimmed(),
        _ => nu_ansi_term::Style::default(),
    }
}

pub(crate) fn git_repo_status_style(symbol: char) -> nu_ansi_term::Style {
    match symbol {
        '+' => nu_ansi_term::Color::Red.normal(),
        '|' => nu_ansi_term::Color::Green.normal(),
        '~' => nu_ansi_term::Color::Yellow.normal(),
        _ => nu_ansi_term::Style::default().dimmed(),
    }
}

pub(crate) fn git_remote_status_style(symbol: char) -> nu_ansi_term::Style {
    match symbol {
        '✓' => nu_ansi_term::Color::Green.normal(),
        '↑' | '↕' => nu_ansi_term::Color::Red.normal(),
        '↓' => nu_ansi_term::Color::Yellow.normal(),
        '?' => nu_ansi_term::Color::White.normal(),
        _ => nu_ansi_term::Style::default().dimmed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_git_status_pairs_to_display_symbols() {
        assert_eq!(parse_git_status_pair("MM"), ('M', 'M'));
        assert_eq!(parse_git_status_pair("??"), (' ', '?'));
        assert_eq!(parse_git_status_pair("UU"), ('U', 'U'));
        assert_eq!(parse_git_status_pair("R "), ('R', ' '));
        assert_eq!(git_status_symbol('?'), 'N');
        assert_eq!(git_status_symbol(' '), '-');
    }
}
