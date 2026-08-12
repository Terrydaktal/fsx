use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    Always,
    Auto,
    Never,
}

impl OutputMode {
    pub fn enabled(self, is_tty: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Auto => is_tty,
            Self::Never => false,
        }
    }
}

pub fn escape_terminal_text(text: &str) -> Cow<'_, str> {
    if !text.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => {
                output.push_str(&format!("\\x{:02x}", character as u32));
            }
            _ => output.push(character),
        }
    }
    Cow::Owned(output)
}

pub fn dim_text(text: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[2m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn encode_file_uri_path(path: &std::path::Path) -> Option<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        crate::path::full_path(path)
    };
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    let bytes = absolute.as_os_str().as_bytes().to_vec();
    #[cfg(not(unix))]
    let bytes = absolute.to_string_lossy().as_bytes().to_vec();
    let mut uri = String::from("file://");
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            byte => {
                uri.push('%');
                uri.push(HEX[(byte >> 4) as usize] as char);
                uri.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    Some(uri)
}

#[derive(Default)]
pub struct HyperlinkCache {
    encoded_paths: HashMap<PathBuf, String>,
    encoded_parent_paths: HashMap<PathBuf, String>,
}

impl HyperlinkCache {
    /// Link a visible entry directly to itself. Directory links must remain
    /// direct so terminal middle-click handlers receive the directory path.
    pub fn direct_link(&mut self, path: &Path, label: &str) -> String {
        let absolute = absolute_path(path);
        match self.encoded_path(&absolute) {
            Some(uri) => osc8_wrap(&uri, label),
            None => label.to_string(),
        }
    }

    /// Explicitly open an entry's parent and select the entry.
    /// Ordinary basename labels should use `direct_link`; selection is reserved
    /// for the dirname portion of a split full path.
    pub fn select_link(&mut self, path: &Path, label: &str) -> String {
        let absolute = absolute_path(path);
        match self.selection_uri(&absolute) {
            Some(uri) => osc8_wrap(&uri, label),
            None => label.to_string(),
        }
    }

    /// Render a path using Unearth's split-link behavior: the visible prefix
    /// selects the entry in its parent, while the basename opens the entry.
    pub fn split_path_link(&mut self, path: &Path, prefix: &str, leaf: &str) -> String {
        self.split_path_link_with_leaf_selection(path, prefix, leaf, false)
    }

    /// Render a split path while optionally making the basename select the
    /// entry in its parent instead of opening the entry directly.
    pub fn split_path_link_with_leaf_selection(
        &mut self,
        path: &Path,
        prefix: &str,
        leaf: &str,
        select_leaf: bool,
    ) -> String {
        let absolute = absolute_path(path);
        let Some(leaf_uri) = self.encoded_path(&absolute) else {
            return format!("{prefix}{leaf}");
        };
        if prefix.is_empty() {
            let uri = if select_leaf {
                self.selection_uri(&absolute).unwrap_or(leaf_uri)
            } else {
                leaf_uri
            };
            return osc8_wrap(&uri, leaf);
        }
        let Some(parent_uri) = self.parent_uri(&absolute) else {
            return format!("{prefix}{leaf}");
        };
        let select_uri = format!("{parent_uri}?select={}", uri_path(&leaf_uri));
        let leaf_link = if select_leaf {
            osc8_wrap(&select_uri, leaf)
        } else {
            osc8_wrap(&leaf_uri, leaf)
        };
        format!(
            "{}{}{}{}",
            osc8_wrap_open(&select_uri),
            prefix,
            osc8_wrap_close(),
            leaf_link,
        )
    }

    fn selection_uri(&mut self, absolute: &Path) -> Option<String> {
        let parent_uri = self.parent_uri(absolute)?;
        let leaf_uri = self.encoded_path(absolute)?;
        Some(format!("{parent_uri}?select={}", uri_path(&leaf_uri)))
    }

    fn parent_uri(&mut self, absolute: &Path) -> Option<String> {
        let parent = absolute.parent().unwrap_or_else(|| Path::new("/"));
        let mut uri = if let Some(uri) = self.encoded_parent_paths.get(parent) {
            uri.clone()
        } else {
            let uri = encode_file_uri_path(parent)?;
            self.encoded_parent_paths
                .insert(parent.to_path_buf(), uri.clone());
            uri
        };
        if !uri.ends_with('/') {
            uri.push('/');
        }
        Some(uri)
    }

    fn encoded_path(&mut self, path: &Path) -> Option<String> {
        if let Some(uri) = self.encoded_paths.get(path) {
            return Some(uri.clone());
        }
        let uri = encode_file_uri_path(path)?;
        self.encoded_paths.insert(path.to_path_buf(), uri.clone());
        if let Some(parent) = path.parent() {
            self.encoded_parent_paths
                .entry(parent.to_path_buf())
                .or_insert_with(|| encode_file_uri_path(parent).unwrap_or_default());
        }
        Some(uri)
    }
}

pub fn osc8_link(path: &Path, label: &str) -> String {
    HyperlinkCache::default().direct_link(path, label)
}

fn absolute_path(path: &Path) -> PathBuf {
    crate::path::normalize_lexical(&crate::path::full_path(path))
}

fn uri_path(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

fn osc8_wrap(uri: &str, label: &str) -> String {
    format!("{}{}{}", osc8_wrap_open(uri), label, osc8_wrap_close())
}

fn osc8_wrap_open(uri: &str) -> String {
    format!("\x1b]8;;{uri}\x1b\\")
}

fn osc8_wrap_close() -> &'static str {
    "\x1b]8;;\x1b\\"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_directory_link_targets_the_directory_itself() {
        let mut cache = HyperlinkCache::default();
        let rendered = cache.direct_link(Path::new("/tmp/fsx/space name"), "space name/");
        assert_eq!(
            rendered,
            "\x1b]8;;file:///tmp/fsx/space%20name\x1b\\space name/\x1b]8;;\x1b\\"
        );
        assert!(!rendered.contains("?select="));
    }

    #[test]
    fn selection_link_targets_parent_and_selects_encoded_entry() {
        let mut cache = HyperlinkCache::default();
        assert_eq!(
            cache.select_link(Path::new("/tmp/fsx/space name"), "space name"),
            "\x1b]8;;file:///tmp/fsx/?select=/tmp/fsx/space%20name\x1b\\space name\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn split_link_matches_unearth_parent_selection_behavior() {
        let mut cache = HyperlinkCache::default();
        assert_eq!(
            cache.split_path_link(Path::new("/tmp/fsx/space name"), "/tmp/fsx/", "space name",),
            "\x1b]8;;file:///tmp/fsx/?select=/tmp/fsx/space%20name\x1b\\/tmp/fsx/\x1b]8;;\x1b\\\x1b]8;;file:///tmp/fsx/space%20name\x1b\\space name\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn split_link_can_select_the_leaf_entry() {
        let mut cache = HyperlinkCache::default();
        assert_eq!(
            cache.split_path_link_with_leaf_selection(
                Path::new("/tmp/fsx/space name"),
                "/tmp/fsx/",
                "space name",
                true,
            ),
            "\x1b]8;;file:///tmp/fsx/?select=/tmp/fsx/space%20name\x1b\\/tmp/fsx/\x1b]8;;\x1b\\\x1b]8;;file:///tmp/fsx/?select=/tmp/fsx/space%20name\x1b\\space name\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn dim_text_uses_the_shared_terminal_style() {
        assert_eq!(dim_text(" 8 Aug 12:30", true), "\x1b[2m 8 Aug 12:30\x1b[0m");
        assert_eq!(dim_text(" 8 Aug 12:30", false), " 8 Aug 12:30");
    }
}
