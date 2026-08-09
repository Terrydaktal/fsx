use regex::Regex;
use std::collections::HashMap;

fn split_ls_colors_entries(spec: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in spec.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == ':' {
            entries.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
    }
    if escaped {
        current.push('\\');
    }
    entries.push(current);
    entries
}

#[derive(Clone)]
pub struct ColorSpec {
    by_key: HashMap<String, String>,
    suffix_globs: HashMap<String, (usize, String)>,
    globs: Vec<(usize, Regex, String)>,
    pub color_prefix_dir: String,
    pub color_dir: String,
    pub color_link: String,
    pub color_exec: String,
}

pub fn parse_ls_colors_value(spec: &str) -> ColorSpec {
    let mut by_key = HashMap::new();
    let mut suffix_globs = HashMap::new();
    let mut globs = Vec::new();
    let (mut color_dir, mut color_link, mut color_exec) = (
        "01;34".to_string(),
        "01;36".to_string(),
        "01;32".to_string(),
    );
    for (order, entry) in split_ls_colors_entries(spec).into_iter().enumerate() {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        if let Some(suffix) = key.strip_prefix('*') {
            if !suffix.contains(['*', '?', '\\']) {
                suffix_globs
                    .entry(suffix.to_string())
                    .or_insert_with(|| (order, value.to_string()));
                continue;
            }
            let mut regex = String::from("^");
            for character in key.chars() {
                match character {
                    '*' => regex.push_str(".*"),
                    '?' => regex.push('.'),
                    '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                        regex.push('\\');
                        regex.push(character);
                    }
                    _ => regex.push(character),
                }
            }
            regex.push('$');
            if let Ok(compiled) = Regex::new(&regex) {
                globs.push((order, compiled, value.to_string()));
            }
        } else {
            by_key.insert(key.to_string(), value.to_string());
            match key {
                "di" => color_dir = value.to_string(),
                "ln" => color_link = value.to_string(),
                "ex" => color_exec = value.to_string(),
                _ => {}
            }
        }
    }
    ColorSpec {
        by_key,
        suffix_globs,
        globs,
        color_prefix_dir: "38;2;255;255;255".to_string(),
        color_dir,
        color_link,
        color_exec,
    }
}

pub fn default_color_spec() -> ColorSpec {
    parse_ls_colors_value("")
}

pub fn color_code_for_path<'a>(
    path: &str,
    is_dir: bool,
    is_symlink: bool,
    target_is_dir: bool,
    executable: bool,
    colors: &'a ColorSpec,
) -> Option<&'a str> {
    let link_code = colors
        .by_key
        .get("ln")
        .map(String::as_str)
        .unwrap_or(colors.color_link.as_str());
    let symlink_target_mode = is_symlink && link_code == "target";
    if is_symlink && !symlink_target_mode {
        return Some(link_code);
    }
    if is_dir || (symlink_target_mode && target_is_dir) {
        return colors
            .by_key
            .get("di")
            .map(String::as_str)
            .or(Some(colors.color_dir.as_str()));
    }

    let base = path.rsplit('/').next().unwrap_or("");
    let mut best: Option<(usize, &str)> = None;
    let mut consider = |order: usize, code: &'a str| {
        if best.is_none_or(|(best_order, _)| order < best_order) {
            best = Some((order, code));
        }
    };
    for (start, _) in base.char_indices() {
        if let Some((order, code)) = colors.suffix_globs.get(&base[start..]) {
            consider(*order, code.as_str());
        }
    }
    if let Some((order, code)) = colors.suffix_globs.get("") {
        consider(*order, code.as_str());
    }
    for (order, regex, value) in &colors.globs {
        if regex.is_match(base) {
            consider(*order, value.as_str());
        }
    }
    best.map(|(_, code)| code)
        // An extension rule is authoritative for executable files with an
        // extension. Extensionless executables use `ex`, then regular files
        // use `fi`.
        .or_else(|| executable.then_some(colors.color_exec.as_str()))
        .or_else(|| colors.by_key.get("fi").map(String::as_str))
}
