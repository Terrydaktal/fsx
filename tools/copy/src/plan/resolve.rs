//! Path, endpoint, mount, and destination-shape resolution.

use crate::domain::{DstObjKind, Endpoint, LogLevel, RemoteSpec, SrcObjKind, TransferMode};
use crate::output::log;
use nix::sys::statvfs::statvfs;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(crate) fn existing_probe_path(path: &Path) -> Option<PathBuf> {
    let mut probe = if path.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        path.to_path_buf()
    };
    loop {
        if fs::symlink_metadata(&probe).is_ok() {
            return Some(probe);
        }
        if !probe.pop() {
            return None;
        }
    }
}

pub(crate) fn destination_available_bytes(path: &Path) -> io::Result<(u64, PathBuf)> {
    let probe = existing_probe_path(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "No existing destination ancestor path found",
        )
    })?;
    let stats = statvfs(&probe)
        .map_err(|e| io::Error::other(format!("statvfs failed for {}: {e}", probe.display())))?;
    let avail = stats
        .blocks_available()
        .saturating_mul(stats.fragment_size() as u64);
    Ok((avail, probe))
}

pub(crate) fn can_fast_rename_same_fs(source: &Path, target: &Path) -> bool {
    source
        .parent()
        .and_then(|p| fs::metadata(p).ok())
        .map(|m| m.dev())
        .zip(
            target
                .parent()
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| m.dev()),
        )
        .map(|(a, b)| a == b)
        .unwrap_or(false)
}

pub(crate) fn expand_user(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    if value == "~" {
        if let Ok(home) = env::var("HOME") {
            return home;
        }
    }
    value.to_string()
}

pub(crate) fn realpath_allow_missing(input: &Path) -> PathBuf {
    let abs = if input.is_absolute() {
        input.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(input)
    };

    // Resolve ancestors, but preserve the final symlink as an object to copy
    // rather than silently turning `copy link destination` into `copy target`.
    if let Ok(meta) = fs::symlink_metadata(&abs) {
        if meta.file_type().is_symlink() {
            let name = abs.file_name().map(PathBuf::from);
            let parent = abs.parent().unwrap_or_else(|| Path::new("."));
            let mut resolved = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            if let Some(name) = name {
                resolved.push(name);
            }
            return resolved;
        }
        return fs::canonicalize(&abs).unwrap_or(abs);
    }

    let mut tail: Vec<PathBuf> = Vec::new();
    let mut cur = abs.clone();
    while fs::symlink_metadata(&cur).is_err() {
        if let Some(name) = cur.file_name() {
            tail.push(PathBuf::from(name));
        }
        if let Some(parent) = cur.parent() {
            cur = parent.to_path_buf();
        } else {
            break;
        }
    }

    let mut resolved = if fs::symlink_metadata(&cur).is_ok() {
        fs::canonicalize(&cur).unwrap_or(cur)
    } else {
        cur
    };
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    resolved
}

pub(crate) fn to_real_path(value: &str) -> PathBuf {
    let expanded = expand_user(value);
    realpath_allow_missing(Path::new(&expanded))
}

pub(crate) fn parse_remote_spec(value: &str) -> Option<RemoteSpec> {
    if value.contains("://") {
        return None;
    }
    let idx = value.find(':')?;
    let lhs = &value[..idx];
    let rhs = &value[idx + 1..];
    if lhs.is_empty() || rhs.is_empty() {
        return None;
    }
    if lhs.contains('/') || lhs.contains('\\') || lhs.contains(char::is_whitespace) {
        return None;
    }
    let (user, host) = match lhs.rfind('@') {
        Some(at) => {
            let u = lhs[..at].trim();
            let h = lhs[at + 1..].trim();
            if u.is_empty() || h.is_empty() {
                return None;
            }
            (Some(u.to_string()), h.to_string())
        }
        None => (None, lhs.to_string()),
    };
    Some(RemoteSpec {
        user,
        host,
        path: rhs.to_string(),
    })
}

pub(crate) fn wildcard_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star: Option<usize> = None;
    let mut match_ti = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            match_ti = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            match_ti += 1;
            ti = match_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub(crate) fn ssh_config_user_for_host_from_text(host: &str, txt: &str) -> Option<String> {
    let mut in_match = true;
    let mut found: Option<String> = None;

    for raw in txt.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let key = match parts.next() {
            Some(k) => k.to_ascii_lowercase(),
            None => continue,
        };
        let val = parts.collect::<Vec<_>>().join(" ");
        if val.is_empty() {
            continue;
        }

        if key == "host" {
            let mut has_positive = false;
            let mut matched_positive = false;
            let mut matched_negative = false;
            for pat in val.split_whitespace() {
                if let Some(neg) = pat.strip_prefix('!') {
                    if !neg.is_empty() && wildcard_match(neg, host) {
                        matched_negative = true;
                    }
                } else {
                    has_positive = true;
                    if wildcard_match(pat, host) {
                        matched_positive = true;
                    }
                }
            }
            in_match = has_positive && matched_positive && !matched_negative;
            continue;
        }

        if in_match && key == "user" && found.is_none() {
            found = Some(val);
        }
    }
    found
}

pub(crate) fn ssh_config_user_for_host(host: &str) -> Option<String> {
    let home = env::var("HOME").ok()?;
    let cfg_path = Path::new(&home).join(".ssh/config");
    let txt = fs::read_to_string(cfg_path).ok()?;
    ssh_config_user_for_host_from_text(host, &txt)
}

pub(crate) fn enrich_remote_spec(mut r: RemoteSpec) -> RemoteSpec {
    if r.user.is_none() {
        r.user = ssh_config_user_for_host(&r.host);
    }
    r
}

pub(crate) fn endpoint_to_rsync(
    endpoint: &Endpoint,
    as_source: bool,
    contents_mode: bool,
    local_src_kind: Option<SrcObjKind>,
) -> String {
    match endpoint {
        Endpoint::Local(p) => {
            let mut s = p.display().to_string();
            if as_source
                && contents_mode
                && matches!(local_src_kind, Some(SrcObjKind::Dir))
                && !s.ends_with('/')
            {
                s.push('/');
            }
            if !as_source && p.is_dir() && !s.ends_with('/') {
                s.push('/');
            }
            s
        }
        Endpoint::Remote(r) => {
            let mut path = r.path.clone();
            if as_source && contents_mode && !path.ends_with('/') {
                path.push('/');
            }
            let user_host = match &r.user {
                Some(u) => format!("{u}@{}", r.host),
                None => r.host.clone(),
            };
            format!("{user_host}:{path}")
        }
    }
}

pub(crate) fn resolve_source(
    value: &str,
    mode: TransferMode,
) -> Result<(PathBuf, SrcObjKind), i32> {
    let p = to_real_path(value);
    let metadata = match fs::symlink_metadata(&p) {
        Ok(metadata) => metadata,
        Err(_) => {
            log(
                mode,
                &format!("Source path does not exist: {value}"),
                LogLevel::Error,
            );
            return Err(1);
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok((p, SrcObjKind::File));
    }
    if !metadata.is_dir() && !metadata.is_file() {
        log(
            mode,
            &format!("Source path must be a file or directory: {value}"),
            LogLevel::Error,
        );
        return Err(1);
    }
    if metadata.is_dir() {
        return Ok((p, SrcObjKind::Dir));
    }
    Ok((p, SrcObjKind::File))
}

pub(crate) fn resolve_destination_for_file(
    value: &str,
    mode: TransferMode,
    replace_dest_symlink: bool,
) -> Result<(PathBuf, DstObjKind), i32> {
    if replace_dest_symlink {
        let dst_real = PathBuf::from(expand_user(value));
        if let Ok(md) = fs::symlink_metadata(&dst_real) {
            if md.file_type().is_symlink() {
                return Ok((dst_real, DstObjKind::File));
            }
            if md.is_dir() {
                return Ok((dst_real, DstObjKind::Dir));
            }
            return Ok((dst_real, DstObjKind::File));
        }
        let parent = dst_real.parent().unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            log(
                mode,
                &format!(
                    "Destination parent directory does not exist: {}",
                    parent.display()
                ),
                LogLevel::Error,
            );
            return Err(1);
        }
        return Ok((dst_real, DstObjKind::File));
    }
    let dst_real = to_real_path(value);
    if fs::symlink_metadata(&dst_real).is_ok() {
        if fs::metadata(&dst_real)
            .map(|md| md.is_dir())
            .unwrap_or(false)
        {
            return Ok((dst_real, DstObjKind::Dir));
        }
        return Ok((dst_real, DstObjKind::File));
    }
    let parent = dst_real.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        log(
            mode,
            &format!(
                "Destination parent directory does not exist: {}",
                parent.display()
            ),
            LogLevel::Error,
        );
        return Err(1);
    }
    Ok((dst_real, DstObjKind::File))
}

pub(crate) fn create_destination_parents(value: &str, mode: TransferMode) -> Result<(), i32> {
    let destination = to_real_path(value);
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if parent.is_dir() {
        return Ok(());
    }

    fs::create_dir_all(parent).map_err(|err| {
        log(
            mode,
            &format!(
                "Could not create destination parent directory {}: {err}",
                parent.display()
            ),
            LogLevel::Error,
        );
        1
    })
}

pub(crate) fn resolve_destination_for_dir(
    value: &str,
    mode: TransferMode,
    allow_existing_file: bool,
) -> Result<(PathBuf, DstObjKind), i32> {
    let p = to_real_path(value);
    if fs::symlink_metadata(&p).is_ok() {
        if !fs::metadata(&p).map(|md| md.is_dir()).unwrap_or(false) {
            if allow_existing_file {
                return Ok((p, DstObjKind::FileExistingForDir));
            }
            log(
                mode,
                &format!("Destination path must be a directory (or a new directory path): {value}"),
                LogLevel::Error,
            );
            return Err(1);
        }
        return Ok((p, DstObjKind::DirExisting));
    }
    let parent = p.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        log(
            mode,
            &format!(
                "Destination parent directory does not exist: {}",
                parent.display()
            ),
            LogLevel::Error,
        );
        return Err(1);
    }
    Ok((p, DstObjKind::DirNew))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_remote_spec_accepts_user_host_path() {
        let spec = parse_remote_spec("alice@nas:/data/photos").expect("remote spec");
        assert_eq!(spec.user.as_deref(), Some("alice"));
        assert_eq!(spec.host, "nas");
        assert_eq!(spec.path, "/data/photos");
    }

    #[test]
    fn parse_remote_spec_accepts_host_path_without_user() {
        let spec = parse_remote_spec("backup:/srv/archive").expect("remote spec");
        assert_eq!(spec.user, None);
        assert_eq!(spec.host, "backup");
        assert_eq!(spec.path, "/srv/archive");
    }

    #[test]
    fn parse_remote_spec_rejects_local_path_with_colon() {
        assert!(parse_remote_spec("/tmp/a:b").is_none());
        assert!(parse_remote_spec("mtp://phone/path").is_none());
    }
    #[test]
    fn ssh_config_parser_uses_first_matching_user() {
        let cfg = r#"
Host box
  User first
Host box
  User second
"#;
        assert_eq!(
            ssh_config_user_for_host_from_text("box", cfg).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn ssh_config_parser_supports_wildcards_and_negation() {
        let cfg = r#"
Host * !blocked
  User wildcard
Host blocked
  User denied
Host dev-*
  User devuser
"#;
        assert_eq!(
            ssh_config_user_for_host_from_text("prod-1", cfg).as_deref(),
            Some("wildcard")
        );
        assert_eq!(
            ssh_config_user_for_host_from_text("dev-a", cfg).as_deref(),
            Some("wildcard")
        );
        assert_eq!(
            ssh_config_user_for_host_from_text("blocked", cfg).as_deref(),
            Some("denied")
        );
    }
}
