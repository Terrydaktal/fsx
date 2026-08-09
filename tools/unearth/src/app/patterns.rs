use super::model::{ContainsAllSpec, NamePattern, Options, SearchDirMode, TypeFlag};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
pub(crate) fn escape_regex_keep_star(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        if "[](){}.^$|+?".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

pub(crate) fn to_regex_fragment(s: &str) -> String {
    let mut x = escape_regex_keep_star(s);
    x = x.replace(r"\*", "__LITERAL_STAR__");
    x = x.replace('*', ".*");
    x.replace("__LITERAL_STAR__", r"\*")
}

pub(crate) fn wildcard_to_regex(pat: &str) -> String {
    let lead_star = pat.starts_with('*');
    let trail_star =
        pat.ends_with('*') && (pat.len() < 2 || pat.as_bytes()[pat.len() - 2] != b'\\');
    let mut rx = to_regex_fragment(pat);
    if !lead_star {
        rx = format!("^{}", rx);
    }
    if !trail_star {
        rx.push('$');
    }
    rx
}

pub(crate) fn is_wrapped_quote(raw: &str) -> bool {
    (raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2)
        || (raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2)
}

pub(crate) fn parse_name_pattern(raw: &str, regex_mode: bool) -> NamePattern {
    let mut out = NamePattern {
        type_flag: None,
        regex: String::new(),
    };
    if is_wrapped_quote(raw) {
        let mut inner = raw[1..raw.len() - 1].to_string();
        inner = inner.trim_start_matches('/').to_string();
        if inner != "/" {
            inner = inner.trim_end_matches('/').to_string();
        }
        out.regex = if regex_mode {
            inner
        } else {
            wildcard_to_regex(&inner)
        };
        return out;
    }
    if regex_mode {
        out.regex = raw.to_string();
        return out;
    }
    if raw.starts_with('/') && raw.ends_with('/') {
        let frag = raw
            .strip_prefix('/')
            .and_then(|value| value.strip_suffix('/'))
            .unwrap_or_default()
            .to_string();
        out.type_flag = Some(TypeFlag::Dir);
        out.regex = format!("^{}$", to_regex_fragment(&frag));
        return out;
    }
    if raw.starts_with('/') {
        let frag = raw.trim_start_matches('/');
        out.regex = format!("^{}", to_regex_fragment(frag));
        return out;
    }
    if raw != "/" && raw.ends_with('/') {
        out.type_flag = Some(TypeFlag::Dir);
        let no_slash = raw.trim_end_matches('/');
        out.regex = format!("{}$", to_regex_fragment(no_slash));
        return out;
    }
    if raw.contains('*') {
        out.regex = wildcard_to_regex(raw);
        return out;
    }
    out.regex = to_regex_fragment(raw);
    out
}

pub(crate) fn pattern_prefers_full_path(raw: &str, regex_mode: bool) -> bool {
    if regex_mode {
        return true;
    }
    let token = if is_wrapped_quote(raw) && raw.len() >= 2 {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    token.contains('/')
}

pub(crate) fn term_selectivity_score(raw: &str, regex_mode: bool) -> i64 {
    let mut score: i64 = 0;
    let core = if is_wrapped_quote(raw) && raw.len() >= 2 {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    let meaningful_len = core
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .count() as i64;
    score += meaningful_len * 24;

    if regex_mode {
        if raw.starts_with('^') {
            score += 1200;
        }
        if raw.ends_with('$') {
            score += 1200;
        }
        if raw.contains(".*") || raw.contains(".+") {
            score -= 900;
        }
        if raw.contains('|') {
            score -= 600;
        }
        let heavy_meta = raw
            .chars()
            .filter(|c| matches!(c, '[' | ']' | '(' | ')' | '{' | '}' | '?' | '+'))
            .count() as i64;
        score -= heavy_meta * 60;
        return score;
    }

    if is_wrapped_quote(raw) {
        score += 2200;
    }
    if raw.starts_with('/') && raw.ends_with('/') {
        score += 1700;
    } else if raw.starts_with('/') || (raw != "/" && raw.ends_with('/')) {
        score += 900;
    }

    let stars = raw.matches('*').count() as i64;
    if stars > 0 {
        score -= stars * 500;
        if raw == "*" {
            score -= 5000;
        }
    } else {
        score += 300;
    }

    score
}

pub(crate) fn canonical_path(raw: &str) -> Option<String> {
    let expanded = expand_home_path(raw);
    let p = Path::new(&expanded);
    if p.is_dir() {
        fs::canonicalize(p)
            .ok()
            .map(|x| x.to_string_lossy().to_string())
    } else {
        None
    }
}

pub(crate) fn parse_search_dir(
    raw: &str,
    regex_mode: bool,
    force_pattern_mode: bool,
) -> SearchDirMode {
    if !force_pattern_mode {
        if let Some(p) = canonical_path(raw) {
            return SearchDirMode::Path(p);
        }
    }
    let mut normalized = raw.to_string();
    if normalized != "/" {
        normalized = normalized.trim_end_matches('/').to_string();
    }
    if is_wrapped_quote(raw) {
        let inner = raw[1..raw.len() - 1].to_string();
        if !force_pattern_mode {
            if let Some(p) = canonical_path(&inner) {
                return SearchDirMode::Path(p);
            }
        }
        let mut pattern_inner = inner.trim_start_matches('/').to_string();
        if pattern_inner != "/" {
            pattern_inner = pattern_inner.trim_end_matches('/').to_string();
        }
        let rx = if regex_mode {
            pattern_inner
        } else {
            wildcard_to_regex(&pattern_inner)
        };
        return SearchDirMode::Pattern(rx);
    }
    if regex_mode {
        return SearchDirMode::Pattern(normalized);
    }
    if raw.starts_with('/') && raw.ends_with('/') {
        let inner = raw
            .strip_prefix('/')
            .and_then(|value| value.strip_suffix('/'))
            .unwrap_or_default();
        return SearchDirMode::Pattern(format!("^{}$", to_regex_fragment(inner)));
    }
    if raw.starts_with("./") && raw.ends_with('/') {
        return SearchDirMode::Pattern(format!("^{}$", to_regex_fragment(&raw[2..raw.len() - 1])));
    }
    if raw.starts_with('/') {
        return SearchDirMode::Pattern(format!(
            "^{}",
            to_regex_fragment(raw.trim_start_matches('/'))
        ));
    }
    if raw.starts_with("./") {
        return SearchDirMode::Pattern(format!(
            "^{}",
            to_regex_fragment(raw.trim_start_matches("./"))
        ));
    }
    if raw != "/" && raw.ends_with('/') {
        return SearchDirMode::Pattern(format!(
            "{}$",
            to_regex_fragment(raw.trim_end_matches('/'))
        ));
    }
    if normalized.contains('*') {
        return SearchDirMode::Pattern(wildcard_to_regex(&normalized));
    }
    SearchDirMode::Pattern(to_regex_fragment(&normalized))
}

pub(crate) fn expand_home_path(raw: &str) -> String {
    if raw == "~" {
        return env::var("HOME").unwrap_or_else(|_| raw.to_string());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{}/{}", home, rest);
        }
    }
    raw.to_string()
}

pub(crate) fn is_implicit_content_path_token(raw: &str) -> bool {
    raw == "."
        || raw == ".."
        || raw == "~"
        || raw.starts_with('/')
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.starts_with("~/")
        || raw.contains('/')
}

pub(crate) fn is_explicit_search_dir_selector(raw: &str) -> bool {
    if raw == "." || raw == ".." || raw == "~" {
        return true;
    }
    if is_wrapped_quote(raw) {
        return true;
    }
    if raw.contains('*') {
        return true;
    }
    if (raw.starts_with('/') || raw.starts_with("./")) && raw.ends_with('/') {
        return true;
    }
    if raw.starts_with('/') || raw.starts_with("./") {
        return true;
    }
    raw != "/" && raw.ends_with('/')
}

pub(crate) fn resolve_literal_search_root(raw: &str) -> Result<PathBuf, String> {
    let expanded = expand_home_path(raw);
    let path = PathBuf::from(expanded);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!(
            "--path target '{}' is not an existing directory",
            raw
        ))
    }
}

pub(crate) fn contains_all_spec_from_opts(
    opts: &Options,
) -> Result<Option<ContainsAllSpec>, String> {
    let implicit_by_terms = if opts.positional.len() >= 3 {
        true
    } else if opts.positional.len() == 2 {
        let first = &opts.positional[0];
        let second = &opts.positional[1];
        !opts.regex_mode
            && !opts.force_pattern_mode
            && !is_wrapped_quote(first)
            && !is_explicit_search_dir_selector(second)
    } else {
        false
    };
    let forced_by_flags = opts.contains_all || opts.path_override.is_some();
    if opts.force_full && !forced_by_flags && !implicit_by_terms {
        return Ok(None);
    }
    if !(forced_by_flags || implicit_by_terms) {
        return Ok(None);
    }
    let mut terms = opts.positional.clone();
    let mut root_raw = opts.path_override.clone();
    if root_raw.is_none() && !terms.is_empty() {
        if let Some(last) = terms.last() {
            if is_implicit_content_path_token(last) {
                root_raw = Some(last.clone());
                terms.pop();
            }
        }
    }
    if terms.is_empty() {
        return Err("contains-all mode requires at least one search term".to_string());
    }
    let root = if let Some(raw) = root_raw {
        resolve_literal_search_root(&raw)?
    } else {
        PathBuf::from(".")
    };
    Ok(Some(ContainsAllSpec { terms, root }))
}
