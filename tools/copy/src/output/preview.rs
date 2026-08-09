//! Preview trees, change summaries, and terminal tree rendering.
#![allow(clippy::too_many_arguments)]

use crate::domain::{ChangeItem, ChangeKind, SrcObjKind};
use crate::output::{ENDC, FAIL, LIGHT_TEAL, OKGREEN, WARNING, WHITE};
use rustc_hash::FxHashMap;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default, Clone)]
pub(crate) struct TreeNode {
    children: FxHashMap<String, TreeNode>,
    state: Option<String>,
    is_dir: bool,
    explicit_added_dir: bool,
}

pub(crate) fn build_change_tree(items: &[ChangeItem]) -> TreeNode {
    let mut root = TreeNode {
        children: FxHashMap::default(),
        state: None,
        is_dir: true,
        explicit_added_dir: false,
    };

    for it in items {
        let mut p = it.rel.trim().trim_start_matches("./").to_string();
        if p.is_empty() {
            continue;
        }
        let leaf_is_dir = p.ends_with('/');
        p = p.trim_end_matches('/').to_string();
        if p.is_empty() {
            continue;
        }
        let parts: Vec<&str> = p.split('/').collect();

        let leaf_state = match it.kind {
            ChangeKind::NewFile | ChangeKind::NewDir => "added",
            ChangeKind::RemovedFile | ChangeKind::RemovedDir => "removed",
            _ => "modified",
        }
        .to_string();

        let mut node = &mut root;
        for (idx, part) in parts.iter().enumerate() {
            let is_leaf = idx == parts.len() - 1;
            node = node
                .children
                .entry((*part).to_string())
                .or_insert_with(|| TreeNode {
                    children: FxHashMap::default(),
                    state: None,
                    is_dir: true,
                    explicit_added_dir: false,
                });

            if is_leaf {
                node.is_dir = leaf_is_dir;
                if it.kind == ChangeKind::NewDir {
                    node.explicit_added_dir = true;
                }
                match leaf_state.as_str() {
                    "added" => {
                        if node.state.is_none() {
                            node.state = Some("added".to_string());
                        }
                    }
                    "removed" => node.state = Some("removed".to_string()),
                    _ => {
                        if node.state.as_deref() != Some("added") {
                            node.state = Some("modified".to_string());
                        }
                    }
                }
            } else {
                node.is_dir = true;
                if node.state.as_deref() != Some("added") {
                    node.state = Some("modified".to_string());
                }
            }
        }
    }

    fn normalize_new_directory_states(node: &mut TreeNode) {
        for child in node.children.values_mut() {
            normalize_new_directory_states(child);
        }
        if node.explicit_added_dir {
            let all_children_added = node
                .children
                .values()
                .all(|child| child.state.as_deref() == Some("added"));
            node.state = Some(
                if all_children_added {
                    "added"
                } else {
                    "modified"
                }
                .to_string(),
            );
        }
    }

    normalize_new_directory_states(&mut root);
    root
}

#[derive(Clone)]
pub(crate) struct LevelEntry<'a> {
    name: String,
    state: String,
    is_dir: bool,
    node: Option<&'a TreeNode>,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct HiddenStateCounts {
    new_count: usize,
    modified_count: usize,
    identical_count: usize,
    uncollided_count: usize,
    deleted_count: usize,
}

impl HiddenStateCounts {
    fn total(self) -> usize {
        self.new_count
            + self.modified_count
            + self.identical_count
            + self.uncollided_count
            + self.deleted_count
    }
}

pub(crate) fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

pub(crate) fn remap_item_under_prefix(item: &str, prefix: &str) -> String {
    if item.is_empty() {
        return format!("{prefix}/");
    }
    if item == prefix || item.starts_with(&format!("{prefix}/")) {
        item.to_string()
    } else {
        format!("{prefix}/{item}")
    }
}

pub(crate) fn remap_path_set_under_prefix(
    paths: &HashSet<String>,
    prefix: &str,
) -> HashSet<String> {
    paths
        .iter()
        .map(|item| {
            remap_item_under_prefix(item, prefix)
                .trim_end_matches('/')
                .to_string()
        })
        .collect()
}

pub(crate) fn state_sort_priority(state: &str) -> u8 {
    match state {
        "modified" | "replaced" => 0,
        "added" => 1,
        "removed" => 2,
        "identical" => 3,
        "uncollided" => 4,
        _ => 9,
    }
}

pub(crate) fn collect_level_entries<'a>(
    abs_dir: &Path,
    node: Option<&'a TreeNode>,
    extra: &HashMap<String, String>,
    source_display_paths: &HashSet<String>,
    rel_prefix: &str,
    dir_cache: &mut HashMap<PathBuf, Vec<(String, bool)>>,
) -> Vec<LevelEntry<'a>> {
    let existing = dir_cache.entry(abs_dir.to_path_buf()).or_insert_with(|| {
        if !abs_dir.is_dir() {
            return Vec::new();
        }
        let mut rows: Vec<(String, bool)> = Vec::new();
        if let Ok(rd) = fs::read_dir(abs_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                rows.push((name, is_dir));
            }
        }
        rows
    });
    let mut all: Vec<String> = Vec::with_capacity(existing.len());
    let mut existing_is_dir: HashMap<String, bool> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::with_capacity(existing.len());
    for (name, is_dir) in existing.iter() {
        seen.insert(name.clone());
        all.push(name.clone());
        existing_is_dir.insert(name.clone(), *is_dir);
    }

    if let Some(n) = node {
        for k in n.children.keys() {
            if seen.insert(k.clone()) {
                all.push(k.clone());
            }
        }
    }
    for name in extra.keys() {
        if seen.insert(name.clone()) {
            all.push(name.clone());
        }
    }

    let mut out = Vec::new();
    for name in all {
        let child = node.and_then(|n| n.children.get(&name));
        let mut state = extra.get(&name).cloned();
        if state.is_none() {
            state = child.and_then(|c| c.state.clone());
        }
        let mut state = state.unwrap_or_else(|| "unchanged".to_string());
        if state == "unchanged" && !source_display_paths.is_empty() {
            let rel = join_rel(rel_prefix, &name);
            state = if source_display_paths.contains(rel.as_str()) {
                "identical".to_string()
            } else {
                "uncollided".to_string()
            };
        }
        let full = abs_dir.join(&name);
        let is_dir = child
            .map(|c| c.is_dir)
            .or_else(|| existing_is_dir.get(&name).copied())
            .unwrap_or_else(|| full.is_dir());
        out.push(LevelEntry {
            name,
            state,
            is_dir,
            node: child,
        });
    }
    out
}

pub(crate) fn select_level_entries<'a>(
    entries: &[LevelEntry<'a>],
    max_entries: usize,
    include_unchanged: bool,
) -> (Vec<LevelEntry<'a>>, HiddenStateCounts) {
    let mut ordered: Vec<LevelEntry> = if include_unchanged {
        entries.to_vec()
    } else {
        entries
            .iter()
            .filter(|e| {
                matches!(
                    e.state.as_str(),
                    "added" | "modified" | "replaced" | "removed"
                )
            })
            .cloned()
            .collect()
    };
    let compare_entries = |a: &LevelEntry<'a>, b: &LevelEntry<'a>| {
        let pa = state_sort_priority(a.state.as_str());
        let pb = state_sort_priority(b.state.as_str());
        pa.cmp(&pb).then(a.name.cmp(&b.name))
    };

    if ordered.len() > max_entries {
        let (_, _, _) = ordered.select_nth_unstable_by(max_entries, compare_entries);
        ordered.truncate(max_entries);
    }
    ordered.sort_unstable_by(compare_entries);
    let selected = ordered;
    let selected_names: HashSet<&str> = selected.iter().map(|e| e.name.as_str()).collect();

    let mut hidden = HiddenStateCounts::default();

    for e in entries {
        if selected_names.contains(e.name.as_str()) {
            continue;
        }
        match e.state.as_str() {
            "added" => hidden.new_count += 1,
            "removed" => hidden.deleted_count += 1,
            "identical" => hidden.identical_count += 1,
            "uncollided" | "unchanged" => hidden.uncollided_count += 1,
            _ => hidden.modified_count += 1,
        }
    }

    (selected, hidden)
}

pub(crate) fn format_entry(entry: &LevelEntry, row_kind: Option<&str>) -> String {
    let suffix = if entry.is_dir { "/" } else { "" };
    if row_kind == Some("replaced_old") {
        return format!("{FAIL}{}{} (old){ENDC}", entry.name, suffix);
    }
    if row_kind == Some("replaced_new") {
        return format!("{OKGREEN}{}{} (new){ENDC}", entry.name, suffix);
    }
    match entry.state.as_str() {
        "removed" => format!("{FAIL}{}{} (removed){ENDC}", entry.name, suffix),
        "added" => format!("{OKGREEN}{}{}{ENDC}", entry.name, suffix),
        "modified" => format!("{WARNING}{}{}{ENDC}", entry.name, suffix),
        "identical" => format!("{LIGHT_TEAL}{}{}{ENDC}", entry.name, suffix),
        "uncollided" => format!("{WHITE}{}{}{ENDC}", entry.name, suffix),
        _ => format!("{WHITE}{}{}{ENDC}", entry.name, suffix),
    }
}

pub(crate) fn render_showall_level(
    abs_dir: &Path,
    node: Option<&TreeNode>,
    prefix: &str,
    extras: &HashMap<String, String>,
    source_display_paths: &HashSet<String>,
    rel_prefix: &str,
    include_unchanged: bool,
    depth: usize,
    max_depth: usize,
    trunc: usize,
    dir_cache: &mut HashMap<PathBuf, Vec<(String, bool)>>,
    out: &mut String,
    max_lines: Option<usize>,
    line_count: &mut usize,
) -> bool {
    let entries = collect_level_entries(
        abs_dir,
        node,
        extras,
        source_display_paths,
        rel_prefix,
        dir_cache,
    );
    let (selected, hidden) = select_level_entries(&entries, trunc, include_unchanged);

    enum Unit<'a> {
        Entry(LevelEntry<'a>, Option<&'static str>),
        Summary(HiddenStateCounts),
    }

    let mut units: Vec<Unit> = Vec::new();
    for entry in selected {
        if entry.state == "replaced" {
            units.push(Unit::Entry(entry.clone(), Some("replaced_old")));
            units.push(Unit::Entry(entry, Some("replaced_new")));
        } else {
            units.push(Unit::Entry(entry, None));
        }
    }

    if hidden.total() > 0 {
        units.push(Unit::Summary(hidden));
    }

    for (idx, unit) in units.iter().enumerate() {
        let last = idx + 1 == units.len();
        let branch = if last { "└── " } else { "├── " };

        match unit {
            Unit::Summary(hidden) => {
                if max_lines.map(|m| *line_count >= m).unwrap_or(false) {
                    return false;
                }
                if let Some(summary) = format_hidden_top_summary(
                    hidden.new_count,
                    hidden.modified_count,
                    hidden.identical_count,
                    hidden.uncollided_count,
                    hidden.deleted_count,
                    !include_unchanged,
                ) {
                    let _ = writeln!(out, "{prefix}{branch}... and {summary}");
                    *line_count += 1;
                }
            }
            Unit::Entry(entry, row_kind) => {
                if max_lines.map(|m| *line_count >= m).unwrap_or(false) {
                    return false;
                }
                let _ = writeln!(out, "{prefix}{branch}{}", format_entry(entry, *row_kind));
                *line_count += 1;
                let should_expand = row_kind.is_none()
                    && entry.is_dir
                    && depth + 1 < max_depth
                    && entry.state != "removed";
                if should_expand {
                    let child_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
                    let child_rel = join_rel(rel_prefix, &entry.name);
                    let empty: HashMap<String, String> = HashMap::new();
                    if !render_showall_level(
                        &abs_dir.join(&entry.name),
                        entry.node,
                        &child_prefix,
                        &empty,
                        source_display_paths,
                        &child_rel,
                        include_unchanged,
                        depth + 1,
                        max_depth,
                        trunc,
                        dir_cache,
                        out,
                        max_lines,
                        line_count,
                    ) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

pub(crate) fn render_showall_tree_to_string_with_cache(
    preview_root: &Path,
    tree: &TreeNode,
    source_display_paths: &HashSet<String>,
    include_unchanged: bool,
    extra_added: &HashSet<String>,
    extra_modified: &HashSet<String>,
    extra_replaced: &HashSet<String>,
    extra_removed: &HashSet<String>,
    max_depth: usize,
    trunc: usize,
    max_lines: Option<usize>,
    dir_cache: &mut HashMap<PathBuf, Vec<(String, bool)>>,
) -> Option<String> {
    let mut root_extra: HashMap<String, String> = HashMap::new();
    for n in extra_added {
        root_extra.insert(n.clone(), "added".to_string());
    }
    for n in extra_modified {
        if root_extra.get(n).map(|s| s.as_str()) != Some("added") {
            root_extra.insert(n.clone(), "modified".to_string());
        }
    }
    for n in extra_replaced {
        root_extra.insert(n.clone(), "replaced".to_string());
    }
    for n in extra_removed {
        root_extra.insert(n.clone(), "removed".to_string());
    }

    let mut out = String::new();
    let mut line_count = 0usize;
    let ok = render_showall_level(
        preview_root,
        Some(tree),
        "",
        &root_extra,
        source_display_paths,
        "",
        include_unchanged,
        0,
        max_depth,
        trunc,
        dir_cache,
        &mut out,
        max_lines,
        &mut line_count,
    );
    if !ok {
        return None;
    }
    Some(out)
}

pub(crate) struct TopPreviewData {
    root: PathBuf,
    top_states: HashMap<String, String>,
    top_is_dir: HashMap<String, bool>,
    unchanged_identical: usize,
    unchanged_uncollided: usize,
}

fn collect_top_level_preview_with_cache(
    preview_root: &Path,
    preview_items: &[ChangeItem],
    source_top_entries: &HashSet<String>,
    include_unchanged: bool,
    extra_added: &HashSet<String>,
    extra_modified: &HashSet<String>,
    extra_replaced: &HashSet<String>,
    extra_removed: &HashSet<String>,
    dir_cache: Option<&mut HashMap<PathBuf, Vec<(String, bool)>>>,
) -> TopPreviewData {
    let root = preview_root.to_path_buf();

    let mut existing_entries: HashSet<String> = HashSet::new();
    if let Some(cache) = dir_cache {
        let entries = cache.entry(root.clone()).or_insert_with(|| {
            if !root.is_dir() {
                return Vec::new();
            }
            fs::read_dir(&root)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_dir = entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false);
                    (name, is_dir)
                })
                .collect()
        });
        for (name, _) in entries.iter() {
            existing_entries.insert(name.clone());
        }
    } else if root.is_dir() {
        if let Ok(rd) = fs::read_dir(&root) {
            for e in rd.flatten() {
                existing_entries.insert(e.file_name().to_string_lossy().to_string());
            }
        }
    }

    let mut top_states: HashMap<String, String> = HashMap::new();
    let mut top_is_dir: HashMap<String, bool> = HashMap::new();
    let mut unchanged_identical = 0usize;
    let mut unchanged_uncollided = 0usize;

    for it in preview_items {
        let mut item = it.rel.trim().trim_start_matches("./").to_string();
        if item.is_empty() {
            continue;
        }
        let is_dir = item.ends_with('/');
        item = item.trim_end_matches('/').to_string();
        if item.is_empty() {
            continue;
        }
        let top = item.split('/').next().unwrap_or("").to_string();
        if top.is_empty() {
            continue;
        }

        if is_dir || item.contains('/') {
            top_is_dir.insert(top.clone(), true);
        } else {
            top_is_dir.entry(top.clone()).or_insert(false);
        }

        let state = match it.kind {
            ChangeKind::NewFile | ChangeKind::NewDir => "added",
            ChangeKind::RemovedFile | ChangeKind::RemovedDir => "removed",
            _ => "modified",
        }
        .to_string();

        let prev = top_states.get(&top).cloned();
        if prev.as_deref() == Some("added") {
            continue;
        }
        if state == "added" || prev.is_none() {
            top_states.insert(top, state);
        } else {
            top_states.insert(top, "modified".to_string());
        }
    }

    for n in extra_added {
        top_states.insert(n.clone(), "added".to_string());
    }
    for n in extra_modified {
        if top_states.get(n).map(|s| s.as_str()) != Some("added") {
            top_states.insert(n.clone(), "modified".to_string());
        }
    }
    for n in extra_replaced {
        top_states.insert(n.clone(), "replaced".to_string());
    }
    for n in extra_removed {
        top_states.insert(n.clone(), "removed".to_string());
    }

    for name in top_states.clone().keys() {
        if top_states.get(name).map(|s| s.as_str()) == Some("added")
            && existing_entries.contains(name)
        {
            top_states.insert(name.clone(), "modified".to_string());
        }
    }

    if include_unchanged {
        for name in &existing_entries {
            if top_states.contains_key(name) {
                continue;
            }
            if source_top_entries.contains(name.as_str()) {
                top_states.insert(name.clone(), "identical".to_string());
            } else {
                top_states.insert(name.clone(), "uncollided".to_string());
            }
        }
    } else {
        for name in &existing_entries {
            if top_states.contains_key(name) {
                continue;
            }
            if source_top_entries.contains(name.as_str()) {
                unchanged_identical += 1;
            } else {
                unchanged_uncollided += 1;
            }
        }
    }

    TopPreviewData {
        root,
        top_states,
        top_is_dir,
        unchanged_identical,
        unchanged_uncollided,
    }
}

pub(crate) fn collect_source_top_entries(
    src_mnt: &Path,
    src_obj_kind: SrcObjKind,
    include_root: bool,
    simple_rename_dst: Option<&str>,
    rename_target_only: Option<&str>,
    rename_target_is_dir: bool,
) -> HashSet<String> {
    let mut out = HashSet::new();
    let rename_target_name = simple_rename_dst
        .or_else(|| {
            if (src_obj_kind == SrcObjKind::Dir && rename_target_is_dir)
                || src_obj_kind == SrcObjKind::File
            {
                rename_target_only
            } else {
                None
            }
        })
        .map(|s| s.trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    match src_obj_kind {
        SrcObjKind::File => {
            if let Some(name) = rename_target_name {
                out.insert(name);
            } else if let Some(name) = src_mnt.file_name().map(|s| s.to_string_lossy().to_string())
            {
                out.insert(name);
            }
        }
        SrcObjKind::Dir => {
            if include_root {
                if let Some(name) = rename_target_name {
                    out.insert(name);
                } else if let Some(name) =
                    src_mnt.file_name().map(|s| s.to_string_lossy().to_string())
                {
                    out.insert(name);
                }
            } else if let Ok(rd) = fs::read_dir(src_mnt) {
                for e in rd.flatten() {
                    out.insert(e.file_name().to_string_lossy().to_string());
                }
            }
        }
    }
    out
}

pub(crate) fn render_changed_top_preview_to_string_with_cache(
    preview_root: &Path,
    preview_items: &[ChangeItem],
    source_top_entries: &HashSet<String>,
    include_unchanged: bool,
    extra_added: &HashSet<String>,
    extra_modified: &HashSet<String>,
    extra_replaced: &HashSet<String>,
    extra_removed: &HashSet<String>,
    max_top_entries: usize,
    mut dir_cache: Option<&mut HashMap<PathBuf, Vec<(String, bool)>>>,
) -> String {
    let d = collect_top_level_preview_with_cache(
        preview_root,
        preview_items,
        source_top_entries,
        include_unchanged,
        extra_added,
        extra_modified,
        extra_replaced,
        extra_removed,
        dir_cache.take(),
    );

    let mut visible_ordered_names: Vec<(&String, &String)> = d.top_states.iter().collect();
    let compare_top = |(name_a, state_a): &(&String, &String),
                       (name_b, state_b): &(&String, &String)| {
        state_sort_priority(state_a.as_str())
            .cmp(&state_sort_priority(state_b.as_str()))
            .then(name_a.cmp(name_b))
    };
    if visible_ordered_names.len() > max_top_entries {
        let (_, _, _) = visible_ordered_names.select_nth_unstable_by(max_top_entries, compare_top);
        visible_ordered_names.truncate(max_top_entries);
    }
    visible_ordered_names.sort_unstable_by(compare_top);

    let visible_names: Vec<String> = visible_ordered_names
        .iter()
        .map(|(name, _)| (*name).clone())
        .collect();
    let visible_set: HashSet<&str> = visible_names.iter().map(String::as_str).collect();

    let mut hidden_new = 0usize;
    let mut hidden_modified = 0usize;
    let mut hidden_identical = d.unchanged_identical;
    let mut hidden_uncollided = d.unchanged_uncollided;
    let mut hidden_removed = 0usize;

    for (name, state) in &d.top_states {
        if visible_set.contains(name.as_str()) {
            continue;
        }
        match state.as_str() {
            "added" => hidden_new += 1,
            "removed" => hidden_removed += 1,
            "identical" => hidden_identical += 1,
            "uncollided" => hidden_uncollided += 1,
            _ => hidden_modified += 1,
        }
    }

    if visible_names.is_empty() {
        let mut out = String::new();
        let _ = writeln!(out, "(no new additions)");
        if let Some(summary) = format_hidden_top_summary(
            hidden_new,
            hidden_modified,
            hidden_identical,
            hidden_uncollided,
            hidden_removed,
            !include_unchanged,
        ) {
            let _ = writeln!(out, "... and {summary}");
        }
        out
    } else {
        let mut out = String::new();
        let mut rows: Vec<(String, String, bool)> = Vec::new();
        for name in visible_names {
            let full = d.root.join(&name);
            let is_dir = d
                .top_is_dir
                .get(&name)
                .copied()
                .unwrap_or_else(|| full.is_dir());
            let state = d
                .top_states
                .get(&name)
                .cloned()
                .unwrap_or_else(|| "modified".to_string());
            if state == "replaced" {
                rows.push(("replaced_old".to_string(), name.clone(), is_dir));
                rows.push(("replaced_new".to_string(), name.clone(), is_dir));
            } else {
                rows.push(("single".to_string(), name.clone(), is_dir));
            }
        }

        for (idx, (kind, name, is_dir)) in rows.iter().enumerate() {
            let last = idx + 1 == rows.len();
            let branch = if last { "└── " } else { "├── " };
            let suffix = if *is_dir { "/" } else { "" };
            let state = d
                .top_states
                .get(name)
                .map(|s| s.as_str())
                .unwrap_or("modified");
            let (color, label) = if kind == "replaced_old" {
                (FAIL, " (old)")
            } else if kind == "replaced_new" {
                (OKGREEN, " (new)")
            } else if state == "removed" {
                (FAIL, " (removed)")
            } else if state == "added" {
                (OKGREEN, "")
            } else if state == "identical" {
                (LIGHT_TEAL, "")
            } else if state == "uncollided" {
                (WHITE, "")
            } else {
                (WARNING, "")
            };
            let _ = writeln!(out, "{branch}{color}{name}{suffix}{label}{ENDC}");
        }
        if let Some(summary) = format_hidden_top_summary(
            hidden_new,
            hidden_modified,
            hidden_identical,
            hidden_uncollided,
            hidden_removed,
            !include_unchanged,
        ) {
            let _ = writeln!(out, "... and {summary}");
        }
        out
    }
}

pub(crate) fn print_changed_top_preview(
    preview_root: &Path,
    preview_items: &[ChangeItem],
    source_top_entries: &HashSet<String>,
    include_unchanged: bool,
    extra_added: &HashSet<String>,
    extra_modified: &HashSet<String>,
    extra_replaced: &HashSet<String>,
    extra_removed: &HashSet<String>,
    max_top_entries: usize,
) {
    print_changed_top_preview_with_cache(
        preview_root,
        preview_items,
        source_top_entries,
        include_unchanged,
        extra_added,
        extra_modified,
        extra_replaced,
        extra_removed,
        max_top_entries,
        None,
    );
}

pub(crate) fn print_changed_top_preview_with_cache(
    preview_root: &Path,
    preview_items: &[ChangeItem],
    source_top_entries: &HashSet<String>,
    include_unchanged: bool,
    extra_added: &HashSet<String>,
    extra_modified: &HashSet<String>,
    extra_replaced: &HashSet<String>,
    extra_removed: &HashSet<String>,
    max_top_entries: usize,
    dir_cache: Option<&mut HashMap<PathBuf, Vec<(String, bool)>>>,
) {
    let out = render_changed_top_preview_to_string_with_cache(
        preview_root,
        preview_items,
        source_top_entries,
        include_unchanged,
        extra_added,
        extra_modified,
        extra_replaced,
        extra_removed,
        max_top_entries,
        dir_cache,
    );
    print!("{out}");
    println!();
}

pub(crate) fn format_hidden_top_summary(
    hidden_new: usize,
    hidden_modified: usize,
    hidden_identical: usize,
    hidden_uncollided: usize,
    hidden_deleted: usize,
    combine_unchanged: bool,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if hidden_new > 0 {
        parts.push(format!("{hidden_new} more new"));
    }
    if hidden_modified > 0 {
        parts.push(format!("{hidden_modified} more modified"));
    }
    if combine_unchanged {
        let combined = hidden_identical + hidden_uncollided;
        if combined > 0 {
            parts.push(format!("{combined} more identical/uncollided"));
        }
    } else {
        if hidden_identical > 0 {
            parts.push(format!("{hidden_identical} more identical"));
        }
        if hidden_uncollided > 0 {
            parts.push(format!("{hidden_uncollided} more uncollided"));
        }
    }
    if hidden_deleted > 0 {
        parts.push(format!("{hidden_deleted} more deleted"));
    }
    if !parts.is_empty() {
        Some(parts.join(" "))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(kind: ChangeKind, rel: &str) -> ChangeItem {
        ChangeItem {
            kind,
            rel: rel.to_string(),
        }
    }

    #[test]
    fn new_directory_with_new_children_stays_added() {
        let tree = build_change_tree(&[
            change(ChangeKind::NewDir, "lancedb/"),
            change(ChangeKind::NewDir, "lancedb/collection.lance/"),
            change(ChangeKind::NewFile, "lancedb/collection.lance/data"),
        ]);

        assert_eq!(
            tree.children
                .get("lancedb")
                .and_then(|node| node.state.as_deref()),
            Some("added")
        );
        assert_eq!(
            tree.children
                .get("lancedb")
                .and_then(|node| node.children.get("collection.lance"))
                .and_then(|node| node.state.as_deref()),
            Some("added")
        );
    }

    #[test]
    fn new_directory_with_a_collision_stays_modified() {
        let tree = build_change_tree(&[
            change(ChangeKind::NewDir, "lancedb/"),
            change(ChangeKind::NewFile, "lancedb/new"),
            change(ChangeKind::ModFile, "lancedb/existing"),
        ]);

        assert_eq!(
            tree.children
                .get("lancedb")
                .and_then(|node| node.state.as_deref()),
            Some("modified")
        );
    }
}

#[cfg(test)]
mod hidden_summary_tests {
    use super::*;
    #[test]
    fn format_hidden_top_summary_uses_requested_order_with_deleted_suffix() {
        let line = format_hidden_top_summary(1, 1, 1, 1, 1, false).expect("summary");
        assert_eq!(
            line,
            "1 more new 1 more modified 1 more identical 1 more uncollided 1 more deleted"
        );
    }

    #[test]
    fn format_hidden_top_summary_omits_zero_categories() {
        let line = format_hidden_top_summary(0, 0, 0, 4, 0, false).expect("summary");
        assert_eq!(line, "4 more uncollided");
    }

    #[test]
    fn format_hidden_top_summary_combines_identical_and_unchanged_when_requested() {
        let line = format_hidden_top_summary(0, 0, 3, 4, 0, true).expect("summary");
        assert_eq!(line, "7 more identical/uncollided");
    }
}
