use std::path::{Component, Path, PathBuf};

/// Encode a Unix path as UTF-8 without lossy replacement.
///
/// SQLite's text columns can hold the result, while `%` escaping keeps the
/// representation reversible and preserves `/` as the database hierarchy
/// separator. Valid UTF-8 remains readable; invalid bytes become `%XX`.
pub fn encode_lossless_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        encode_lossless_bytes(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().replace('%', "%25")
    }
}

#[cfg(unix)]
fn encode_lossless_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'/' {
            output.push('/');
            index += 1;
        } else if byte == b'%' {
            output.push_str("%25");
            index += 1;
        } else if byte.is_ascii() {
            output.push(byte as char);
            index += 1;
        } else if let Ok(value) = std::str::from_utf8(&bytes[index..]) {
            let character = value.chars().next().expect("non-empty UTF-8 slice");
            output.push(character);
            index += character.len_utf8();
        } else {
            use std::fmt::Write;
            let _ = write!(output, "%{byte:02X}");
            index += 1;
        }
    }
    output
}

/// Decode a path produced by [`encode_lossless_path`].
pub fn decode_lossless_path(encoded: &str) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = Vec::with_capacity(encoded.len());
        let encoded_bytes = encoded.as_bytes();
        let mut index = 0;
        while index < encoded_bytes.len() {
            if encoded_bytes[index] == b'%' && index + 2 < encoded_bytes.len() {
                let high = (encoded_bytes[index + 1] as char).to_digit(16);
                let low = (encoded_bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    bytes.push(((high << 4) | low) as u8);
                    index += 3;
                    continue;
                }
            }
            let character = encoded[index..]
                .chars()
                .next()
                .expect("non-empty encoded slice");
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            index += character.len_utf8();
        }
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(encoded.replace("%25", "%"))
    }
}

pub fn normalize_lexical(path: &Path) -> PathBuf {
    let absolute = path.is_absolute();
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop_normal = output
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)));
                if can_pop_normal {
                    output.pop();
                } else if !absolute {
                    output.push(component.as_os_str());
                }
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                output.push(component.as_os_str())
            }
        }
    }
    if output.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        output
    }
}

pub fn realpath_allow_missing(path: &Path) -> PathBuf {
    realpath_with_policy(path, false)
}

/// Resolve existing ancestors while optionally preserving an existing final symlink.
pub fn realpath_preserve_final_symlink(path: &Path) -> PathBuf {
    realpath_with_policy(path, true)
}

fn realpath_with_policy(path: &Path, preserve_final_symlink: bool) -> PathBuf {
    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let path = anchored.as_path();

    if preserve_final_symlink
        && let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        let name = path.file_name().map(PathBuf::from);
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        let mut resolved =
            std::fs::canonicalize(parent).unwrap_or_else(|_| normalize_lexical(parent));
        if let Some(name) = name {
            resolved.push(name);
        }
        return resolved;
    }

    if path.exists() {
        return std::fs::canonicalize(path).unwrap_or_else(|_| normalize_lexical(path));
    }
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        let Some(name) = cursor.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = cursor.parent() else { break };
        cursor = parent;
    }
    let mut result = std::fs::canonicalize(cursor).unwrap_or_else(|_| normalize_lexical(cursor));
    for name in missing.iter().rev() {
        result.push(name);
    }
    result
}

pub fn full_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}
