use super::super::presentation::{
    cache_raw_record_path, can_stream_direct, escape_terminal_text, final_transform, style_enabled,
};
use super::*;
use regex::RegexBuilder;
use std::io::{self, BufReader, BufWriter, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;

const QUERY_CONSUMER_STOP: &str = "unearth query consumer stopped after limit";

fn needs_recursive_indexed_dirsize(opts: &Options) -> bool {
    opts.sizes
        || opts.long_extended
        || matches!(opts.sort_field, Some(SortField::Size) if !opts.no_recurse)
}

/// Populate the presentation cache from a clean watcher-backed index.
///
/// A missing regular-file size makes a subtree unsafe to aggregate, so that
/// subtree is deliberately left uncached and the existing filesystem walker
/// remains the fallback.
pub(crate) fn populate_indexed_dirsize_cache(
    conn: &Connection,
    root_key: &str,
    items: &[SearchResult],
    opts: &Options,
    cache: &mut DirStatsCache,
) -> Result<(), String> {
    if !needs_recursive_indexed_dirsize(opts) {
        return Ok(());
    }

    let root_prefix = index_path_prefix(root_key);
    let mut needed_dirs = items
        .iter()
        .filter(|item| should_use_recursive_dirsize(item, opts))
        .map(|item| normalize_dir_key(&item.path))
        .filter(|path| !should_skip_root_size_tree(path))
        .filter(|path| path == root_key || path.starts_with(&root_prefix))
        .collect::<Vec<_>>();
    needed_dirs.sort_unstable();
    needed_dirs.dedup();

    let mut stmt = conn
        .prepare_cached(
            "SELECT
                 COALESCE(SUM(CASE WHEN e.kind = 0 THEN e.size ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN e.kind = 0 THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN e.kind = 0 AND e.size IS NULL THEN 1 ELSE 0 END), 0)
             FROM dirs d
             JOIN entries e INDEXED BY idx_entries_dir ON e.dir_id = d.id
             WHERE d.path = ?1 OR (d.path >= ?2 AND d.path < ?3)",
        )
        .map_err(|e| e.to_string())?;

    for path in needed_dirs {
        let prefix = index_path_prefix(&path);
        let prefix_end = format!("{}0", prefix.trim_end_matches('/'));
        let (bytes, files, missing_sizes): (i64, i64, i64) = stmt
            .query_row(rusqlite::params![path, prefix, prefix_end], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| e.to_string())?;
        if missing_sizes != 0 {
            continue;
        }
        let bytes =
            u64::try_from(bytes).map_err(|_| "indexed directory size overflow".to_string())?;
        let files = u64::try_from(files).map_err(|_| "indexed file count overflow".to_string())?;
        let stats = DirStats {
            files,
            bytes,
            human: format_size_iec(bytes),
        };
        cache.bytes_map.insert(path.clone(), bytes);
        cache.map.insert(path, stats);
    }
    Ok(())
}

pub(crate) fn write_binary_path_record<W: Write>(
    writer: &mut W,
    path: &str,
    path_encoded: bool,
) -> io::Result<()> {
    // SQLite stores lossless paths as escaped UTF-8 keys. Binary consumers
    // expect the original OS bytes, not the database representation.
    let decoded = path_encoded.then(|| fsx::decode_lossless_path(path));
    let bytes = decoded
        .as_deref()
        .map(|path| path.as_os_str().as_bytes())
        .unwrap_or_else(|| path.as_bytes());
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "path too long"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(bytes)
}
pub(crate) fn path_has_hidden_component_below(path: &str, root: &str) -> bool {
    let path = path.trim_end_matches('/');
    let root = root.trim_end_matches('/');
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .trim_start_matches('/');
    relative
        .split('/')
        .any(|part| part.len() > 1 && part.starts_with('.'))
}

pub(crate) fn indexed_root_from_opts(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
) -> Result<(PathBuf, Vec<String>), String> {
    if let Some(spec) = content_spec {
        return Ok((spec.root.clone(), spec.terms.clone()));
    }
    let mut terms = opts.positional.clone();
    let raw_terms = if opts.positional_os.len() == opts.positional.len() {
        Some(opts.positional_os.as_slice())
    } else {
        None
    };
    let root = if terms.len() > 1 {
        let last = terms.last().cloned().unwrap();
        let last_path = raw_terms
            .and_then(|values| values.last())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(expand_home_path(&last)));
        if is_implicit_content_path_token(&last) || last_path.is_dir() {
            terms.pop();
            last_path
        } else {
            PathBuf::from(".")
        }
    } else {
        PathBuf::from(".")
    };
    if terms.is_empty() {
        return Err("--index requires at least one search term".to_string());
    }
    Ok((root, terms))
}

pub(crate) fn recent_query_from_opts(opts: &Options) -> Result<(PathBuf, Vec<String>), String> {
    if let Some(path) = opts.path_override.as_deref() {
        return Ok((
            opts.path_override_os
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(expand_home_path(path))),
            opts.positional.clone(),
        ));
    }
    let mut terms = opts.positional.clone();
    let root = terms.last().and_then(|token| {
        let raw = (opts.positional_os.len() == opts.positional.len())
            .then(|| PathBuf::from(opts.positional_os.last().expect("matching raw operand")));
        if is_implicit_content_path_token(token) || raw.as_ref().is_some_and(|path| path.is_dir()) {
            Some(raw.unwrap_or_else(|| PathBuf::from(expand_home_path(token))))
        } else {
            None
        }
    });
    if root.is_some() {
        terms.pop();
    }
    Ok((root.unwrap_or_else(|| PathBuf::from(".")), terms))
}

pub(crate) fn recent_refresh_threads(opts: &Options, root: &Path) -> usize {
    if opts.threads_explicit {
        return opts.threads_override;
    }
    if root_prefers_single_thread(root) {
        return 1;
    }
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(opts.threads_override)
        .clamp(1, 16)
}

pub(crate) fn run_recent_indexed(
    opts: &Options,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    if let Some(result) = run_indexed_via_daemon(opts, None, cache, colors)? {
        return Ok(result);
    }
    let limit = opts
        .recent_limit
        .ok_or_else(|| "--recent requires a positive integer".to_string())?;
    let (root_raw, terms) = recent_query_from_opts(opts)?;
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let root_prefix = index_path_prefix(&root_key);
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let conn = open_index_db_for_search()?;
    let covering_root = covering_index_root(&conn, &root_key)?;
    let refresh_root = covering_root.as_deref().unwrap_or(&root_key).to_string();
    let live = watch_state_covers_root(&conn, &root_key)?;
    drop(conn);
    if !live {
        let mut refresh_opts = opts.clone();
        refresh_opts.threads_override = recent_refresh_threads(opts, Path::new(&refresh_root));
        refresh_index_root(&refresh_root, &refresh_opts)?;
    }
    let conn = open_index_db_for_search()?;
    let fts_ready = index_search_is_ready(&conn);
    let mut regexes = Vec::with_capacity(terms.len());
    for term in &terms {
        let parsed = parse_name_pattern(term, opts.regex_mode);
        regexes.push(
            RegexBuilder::new(&parsed.regex)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid regex: {}", e))?,
        );
    }

    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let order = opts.sort_order.unwrap_or(SortOrder::Desc);
    let order_sql = match order {
        SortOrder::Asc => "ASC",
        SortOrder::Desc => "DESC",
    };
    let base_name_expr = "lower(s.value)";
    let full_path_expr =
        "lower((CASE WHEN d.path = '/' THEN '/' || s.value ELSE d.path || '/' || s.value END) || CASE WHEN e.kind = 1 THEN '/' ELSE '' END)";
    let sql_field_expr = if opts.force_full {
        full_path_expr
    } else {
        base_name_expr
    };
    let mut sql = String::from(
        "SELECT d.path, s.value, e.activity, e.size
         FROM entries e INDEXED BY idx_entries_kind_activity
         JOIN dirs d ON e.dir_id = d.id
         JOIN strings s ON e.name_id = s.id
         WHERE e.kind = ?
           AND e.activity IS NOT NULL
           AND (d.path = ? OR (d.path >= ? AND d.path < ?))",
    );
    let mut term_params = Vec::<String>::new();
    for term in &terms {
        if let Some((condition, extra_params)) = sql_prefilter_for_term(
            term,
            opts.regex_mode,
            opts.force_full,
            sql_field_expr,
            fts_ready,
        ) {
            sql.push_str(" AND (");
            sql.push_str(&condition);
            sql.push(')');
            term_params.extend(extra_params);
        }
    }
    sql.push_str(&format!(
        " ORDER BY e.activity {order_sql} LIMIT ? OFFSET ?"
    ));
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;

    let kinds: &[i64] = if opts.force_file {
        &[0]
    } else if opts.force_dir {
        &[1]
    } else {
        &[0, 1, 2]
    };
    let mut results = Vec::with_capacity(limit.saturating_mul(kinds.len()));
    let batch_size = limit.max(64).saturating_mul(4);
    for &kind in kinds {
        let mut offset = 0usize;
        let mut visible_for_kind = 0usize;
        while visible_for_kind < limit {
            let mut query_params = Vec::<rusqlite::types::Value>::with_capacity(
                6usize.saturating_add(term_params.len()),
            );
            query_params.push(kind.into());
            query_params.push(root_key.clone().into());
            query_params.push(root_prefix.clone().into());
            query_params.push(root_prefix_end.clone().into());
            query_params.extend(term_params.iter().cloned().map(Into::into));
            query_params.push(
                i64::try_from(batch_size)
                    .map_err(|_| "recent batch too large".to_string())?
                    .into(),
            );
            query_params.push(
                i64::try_from(offset)
                    .map_err(|_| "recent offset too large".to_string())?
                    .into(),
            );
            let rows = stmt
                .query_map(params_from_iter(query_params.iter()), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?;

            let mut fetched = 0usize;
            for row in rows {
                let (dir_path, name, activity, size) = row.map_err(|e| e.to_string())?;
                fetched += 1;
                let mut path = if dir_path == "/" {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", dir_path, name)
                };
                if kind == 1 {
                    path.push('/');
                }
                if is_root_index_excluded_path(&root_key, &path) {
                    continue;
                }
                if opts.visible_only && path_has_hidden_component_below(&path, &root_key) {
                    continue;
                }
                let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                let matched = if opts.force_full {
                    regexes.iter().all(|regex| regex.is_match(&path))
                } else {
                    regexes.iter().all(|regex| regex.is_match(base))
                };
                if !matched
                    || (opts.force_full
                        && regexes.len() > 1
                        && !regexes.iter().any(|regex| regex.is_match(base)))
                {
                    continue;
                }
                results.push(SearchResult {
                    path,
                    path_encoded: true,
                    is_dir: kind == 1,
                    is_symlink: kind == 2,
                    metadata: None,
                    indexed_activity_nanos: activity,
                    indexed_size: size.and_then(|value| u64::try_from(value).ok()),
                });
                visible_for_kind += 1;
                if visible_for_kind >= limit {
                    break;
                }
            }
            if fetched < batch_size {
                break;
            }
            offset = offset.saturating_add(batch_size);
        }
    }
    results.sort_unstable_by(|a, b| {
        let a_activity = a.indexed_activity_nanos.unwrap_or(0);
        let b_activity = b.indexed_activity_nanos.unwrap_or(0);
        match order {
            SortOrder::Asc => a_activity
                .cmp(&b_activity)
                .then_with(|| a.is_dir.cmp(&b.is_dir))
                .then_with(|| a.path.cmp(&b.path)),
            SortOrder::Desc => b_activity
                .cmp(&a_activity)
                .then_with(|| a.is_dir.cmp(&b.is_dir))
                .then_with(|| a.path.cmp(&b.path)),
        }
    });
    results.truncate(limit);

    if live {
        let _ = populate_indexed_dirsize_cache(&conn, &root_key, &results, opts, cache);
    }

    Ok(SearchRun {
        lines: final_transform(results, opts, use_style, stdout_is_tty, colors, cache, None),
        timed_out: false,
    })
}

#[cfg(feature = "watcher")]
pub(crate) fn query_recent_rows<F>(
    opts: &Options,
    conn: &Connection,
    mut emit: F,
) -> Result<(), String>
where
    F: FnMut(SearchResult) -> Result<(), String>,
{
    let limit = opts
        .recent_limit
        .ok_or_else(|| "--recent requires a positive integer".to_string())?;
    let (root_raw, terms) = recent_query_from_opts(opts)?;
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let root_prefix = index_path_prefix(&root_key);
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let fts_ready = index_search_is_ready(conn);
    let mut regexes = Vec::with_capacity(terms.len());
    for term in &terms {
        let parsed = parse_name_pattern(term, opts.regex_mode);
        regexes.push(
            RegexBuilder::new(&parsed.regex)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid regex: {}", e))?,
        );
    }
    let order = opts.sort_order.unwrap_or(SortOrder::Desc);
    let order_sql = match order {
        SortOrder::Asc => "ASC",
        SortOrder::Desc => "DESC",
    };
    let base_name_expr = "lower(s.value)";
    let full_path_expr =
        "lower((CASE WHEN d.path = '/' THEN '/' || s.value ELSE d.path || '/' || s.value END) || CASE WHEN e.kind = 1 THEN '/' ELSE '' END)";
    let sql_field_expr = if opts.force_full {
        full_path_expr
    } else {
        base_name_expr
    };
    let mut sql = String::from(
        "SELECT d.path, s.value, e.activity, e.size
         FROM entries e INDEXED BY idx_entries_kind_activity
         JOIN dirs d ON e.dir_id = d.id
         JOIN strings s ON e.name_id = s.id
         WHERE e.kind = ?
           AND e.activity IS NOT NULL
           AND (d.path = ? OR (d.path >= ? AND d.path < ?))",
    );
    let mut term_params = Vec::<String>::new();
    for term in &terms {
        if let Some((condition, extra_params)) = sql_prefilter_for_term(
            term,
            opts.regex_mode,
            opts.force_full,
            sql_field_expr,
            fts_ready,
        ) {
            sql.push_str(" AND (");
            sql.push_str(&condition);
            sql.push(')');
            term_params.extend(extra_params);
        }
    }
    sql.push_str(&format!(
        " ORDER BY e.activity {order_sql} LIMIT ? OFFSET ?"
    ));
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let kinds: &[i64] = if opts.force_file {
        &[0]
    } else if opts.force_dir {
        &[1]
    } else {
        &[0, 1, 2]
    };
    let batch_size = limit.max(64).saturating_mul(4);
    for &kind in kinds {
        let mut offset = 0usize;
        let mut visible_for_kind = 0usize;
        while visible_for_kind < limit {
            let mut query_params = Vec::<rusqlite::types::Value>::with_capacity(
                6usize.saturating_add(term_params.len()),
            );
            query_params.push(kind.into());
            query_params.push(root_key.clone().into());
            query_params.push(root_prefix.clone().into());
            query_params.push(root_prefix_end.clone().into());
            query_params.extend(term_params.iter().cloned().map(Into::into));
            query_params.push(
                i64::try_from(batch_size)
                    .map_err(|_| "recent batch too large".to_string())?
                    .into(),
            );
            query_params.push(
                i64::try_from(offset)
                    .map_err(|_| "recent offset too large".to_string())?
                    .into(),
            );
            let rows = stmt
                .query_map(params_from_iter(query_params.iter()), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            let mut fetched = 0usize;
            for row in rows {
                let (dir_path, name, activity, size) = row.map_err(|e| e.to_string())?;
                fetched += 1;
                let mut path = if dir_path == "/" {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", dir_path, name)
                };
                if kind == 1 {
                    path.push('/');
                }
                if is_root_index_excluded_path(&root_key, &path) {
                    continue;
                }
                if opts.visible_only && path_has_hidden_component_below(&path, &root_key) {
                    continue;
                }
                let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                let matched = if opts.force_full {
                    regexes.iter().all(|regex| regex.is_match(&path))
                } else {
                    regexes.iter().all(|regex| regex.is_match(base))
                };
                if !matched
                    || (opts.force_full
                        && regexes.len() > 1
                        && !regexes.iter().any(|regex| regex.is_match(base)))
                {
                    continue;
                }
                emit(SearchResult {
                    path,
                    path_encoded: true,
                    is_dir: kind == 1,
                    is_symlink: kind == 2,
                    metadata: None,
                    indexed_activity_nanos: activity,
                    indexed_size: size.and_then(|value| u64::try_from(value).ok()),
                })?;
                visible_for_kind += 1;
                if visible_for_kind >= limit {
                    break;
                }
            }
            if fetched < batch_size {
                break;
            }
            offset = offset.saturating_add(batch_size);
        }
    }
    Ok(())
}

#[cfg(feature = "watcher")]
pub(crate) fn query_indexed_rows<F>(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
    conn: &Connection,
    mut emit: F,
) -> Result<(), String>
where
    F: FnMut(SearchResult) -> Result<(), String>,
{
    let (root_raw, terms) = indexed_root_from_opts(opts, content_spec)?;
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let root_prefix = index_path_prefix(&root_key);
    let mut type_flag = if opts.force_dir {
        Some(TypeFlag::Dir)
    } else if opts.force_file {
        Some(TypeFlag::File)
    } else {
        None
    };
    let mut regexes = Vec::new();
    for term in &terms {
        let parsed = parse_name_pattern(term, opts.regex_mode);
        if parsed.type_flag == Some(TypeFlag::Dir) && !opts.force_file {
            type_flag = Some(TypeFlag::Dir);
        }
        regexes.push(
            RegexBuilder::new(&parsed.regex)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid regex: {}", e))?,
        );
    }
    let fts_ready = index_search_is_ready(conn);
    let base_name_expr = "lower(s.value)";
    let full_path_expr =
        "lower((CASE WHEN d.path = '/' THEN '/' || s.value ELSE d.path || '/' || s.value END) || CASE WHEN e.kind = 1 THEN '/' ELSE '' END)";
    let sql_field_expr = if opts.force_full {
        full_path_expr
    } else {
        base_name_expr
    };
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let mut sql = String::from(
        "SELECT d.path, s.value, e.kind, e.mtime, e.size, e.activity
         FROM entries e
         JOIN dirs d ON e.dir_id = d.id
         JOIN strings s ON e.name_id = s.id
         WHERE (d.path = ? OR (d.path >= ? AND d.path < ?))",
    );
    let mut sql_params = vec![root_key.clone(), root_prefix.clone(), root_prefix_end];
    match type_flag {
        Some(TypeFlag::Dir) => sql.push_str(" AND e.kind = 1"),
        Some(TypeFlag::File) => sql.push_str(" AND e.kind != 1"),
        None => {}
    }
    for term in &terms {
        if let Some((condition, extra_params)) = sql_prefilter_for_term(
            term,
            opts.regex_mode,
            opts.force_full,
            sql_field_expr,
            fts_ready,
        ) {
            sql.push_str(" AND ");
            sql.push('(');
            sql.push_str(&condition);
            sql.push(')');
            sql_params.extend(extra_params);
        }
    }
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params_from_iter(sql_params.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    for row in rows {
        let (dir_path, name, kind, mtime, size, activity) = row.map_err(|e| e.to_string())?;
        let is_dir = kind == 1;
        let is_symlink = kind == 2;
        if matches!(type_flag, Some(TypeFlag::Dir)) && !is_dir {
            continue;
        }
        if matches!(type_flag, Some(TypeFlag::File)) && is_dir {
            continue;
        }
        let mut path = if dir_path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", dir_path, name)
        };
        if is_dir {
            path.push('/');
        }
        if is_root_index_excluded_path(&root_key, &path) {
            continue;
        }
        if opts.visible_only && path_has_hidden_component_below(&path, &root_key) {
            continue;
        }
        let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let matched = if opts.force_full {
            regexes.iter().all(|regex| regex.is_match(&path))
        } else {
            regexes.iter().all(|regex| regex.is_match(base))
        };
        if !matched
            || (opts.force_full
                && regexes.len() > 1
                && !regexes.iter().any(|regex| regex.is_match(base)))
        {
            continue;
        }
        emit(SearchResult {
            path,
            path_encoded: true,
            is_dir,
            is_symlink,
            metadata: None,
            indexed_activity_nanos: activity.or(mtime),
            indexed_size: size.and_then(|value| u64::try_from(value).ok()),
        })?;
    }
    Ok(())
}

pub(crate) fn begin_query_results(
    stream: UnixStream,
) -> Result<Option<BufReader<UnixStream>>, String> {
    let mut reader = BufReader::new(stream);
    let mut magic = [0u8; 8];
    if reader.read_exact(&mut magic).is_err() || &magic != QUERY_RESPONSE_MAGIC {
        return Ok(None);
    }
    let mut status = [0u8; 1];
    if reader.read_exact(&mut status).is_err() {
        return Ok(None);
    }
    if status[0] != 0 {
        return Err(read_query_string(&mut reader)?);
    }
    Ok(Some(reader))
}

pub(crate) fn read_query_results<F>(
    reader: &mut BufReader<UnixStream>,
    mut emit: F,
) -> Result<(), String>
where
    F: FnMut(SearchResult) -> Result<(), String>,
{
    loop {
        let mut length = [0u8; 4];
        reader.read_exact(&mut length).map_err(|e| e.to_string())?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 {
            break;
        }
        if !(17..=QUERY_MAX_FRAME).contains(&length) {
            return Err("invalid fsxd query result frame".to_string());
        }
        let mut frame = vec![0u8; length];
        reader.read_exact(&mut frame).map_err(|e| e.to_string())?;
        let is_dir = frame[0] & 1 != 0;
        let is_symlink = frame[0] & 2 != 0;
        let activity = i64::from_le_bytes(
            frame[1..9]
                .try_into()
                .map_err(|_| "invalid activity field".to_string())?,
        );
        let size = i64::from_le_bytes(
            frame[9..17]
                .try_into()
                .map_err(|_| "invalid size field".to_string())?,
        );
        let path = String::from_utf8(frame[17..].to_vec()).map_err(|e| e.to_string())?;
        emit(SearchResult {
            path,
            path_encoded: true,
            is_dir,
            is_symlink,
            metadata: None,
            indexed_activity_nanos: (activity != i64::MIN).then_some(activity),
            indexed_size: (size >= 0 && size != i64::MIN).then_some(size as u64),
        })?;
    }
    Ok(())
}

pub(crate) fn run_indexed_via_daemon(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<Option<SearchRun>, String> {
    let socket =
        query_socket_path().ok_or_else(|| "Could not determine fsx cache dir".to_string())?;
    if !fs::symlink_metadata(&socket)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let (root_raw, terms) = if opts.recent_limit.is_some() {
        recent_query_from_opts(opts)?
    } else {
        indexed_root_from_opts(opts, content_spec)?
    };
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let status_conn = match open_index_db_for_search() {
        Ok(conn) => conn,
        Err(_) => return Ok(None),
    };
    if !watch_state_covers_root(&status_conn, &root_key)? {
        return Ok(None);
    }
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(_) => return Ok(None),
    };
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(opts.timeout_dur))
        .map_err(|e| e.to_string())?;
    if write_query_request(&mut stream, opts, &root, &terms).is_err() {
        return Ok(None);
    }
    let mut reader = match begin_query_results(stream)? {
        Some(reader) => reader,
        None => return Ok(None),
    };
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    if opts.recent_limit.is_none() && can_stream_direct(opts, use_style) {
        let stdout = io::stdout();
        let mut output = BufWriter::with_capacity(64 * 1024, stdout.lock());
        let mut cache_state = if opts.cache_output {
            init_raw_cache_state()
        } else {
            None
        };
        let mut emitted = 0usize;
        let result = read_query_results(&mut reader, |result| {
            if opts.limit.is_some_and(|limit| emitted >= limit) {
                return Err(QUERY_CONSUMER_STOP.to_string());
            }
            if let Some(state) = cache_state.as_mut() {
                cache_raw_record_path(&result.path, result.is_dir, result.path_encoded, state);
            }
            if opts.index_binary {
                write_binary_path_record(&mut output, &result.path, result.path_encoded)
                    .map_err(|e| e.to_string())?;
            } else {
                output
                    .write_all(escape_terminal_text(&result.path).as_bytes())
                    .and_then(|_| output.write_all(b"\n"))
                    .map_err(|e| e.to_string())?;
            }
            emitted += 1;
            Ok(())
        });
        if let Err(error) = result {
            if error != QUERY_CONSUMER_STOP {
                return Err(format!("indexed query failed after output: {error}"));
            }
        }
        output.flush().map_err(|e| e.to_string())?;
        if let Some(mut state) = cache_state {
            let _ = state.cache.flush();
        }
        return Ok(Some(SearchRun {
            lines: Vec::new(),
            timed_out: false,
        }));
    }
    let mut results = Vec::new();
    read_query_results(&mut reader, |result| {
        results.push(result);
        Ok(())
    })
    .map_err(|error| format!("indexed query failed: {error}"))?;
    if let Some(limit) = opts.recent_limit {
        let order = opts.sort_order.unwrap_or(SortOrder::Desc);
        results.sort_unstable_by(|a, b| {
            let a_activity = a.indexed_activity_nanos.unwrap_or(0);
            let b_activity = b.indexed_activity_nanos.unwrap_or(0);
            match order {
                SortOrder::Asc => a_activity
                    .cmp(&b_activity)
                    .then_with(|| a.is_dir.cmp(&b.is_dir))
                    .then_with(|| a.path.cmp(&b.path)),
                SortOrder::Desc => b_activity
                    .cmp(&a_activity)
                    .then_with(|| a.is_dir.cmp(&b.is_dir))
                    .then_with(|| a.path.cmp(&b.path)),
            }
        });
        results.truncate(limit);
    } else {
        results.sort_by(|a, b| a.path.cmp(&b.path));
    }
    let _ = populate_indexed_dirsize_cache(&status_conn, &root_key, &results, opts, cache);
    Ok(Some(SearchRun {
        lines: final_transform(results, opts, use_style, stdout_is_tty, colors, cache, None),
        timed_out: false,
    }))
}

pub(crate) fn run_indexed(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    if let Some(result) = run_indexed_via_daemon(opts, content_spec, cache, colors)? {
        return Ok(result);
    }
    let (root_raw, terms) = indexed_root_from_opts(opts, content_spec)?;
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let root_prefix = index_path_prefix(&root_key);
    let mut type_flag = if opts.force_dir {
        Some(TypeFlag::Dir)
    } else if opts.force_file {
        Some(TypeFlag::File)
    } else {
        None
    };
    let mut regexes = Vec::new();
    for term in &terms {
        let parsed = parse_name_pattern(term, opts.regex_mode);
        if parsed.type_flag == Some(TypeFlag::Dir) && !opts.force_file {
            type_flag = Some(TypeFlag::Dir);
        }
        regexes.push(
            RegexBuilder::new(&parsed.regex)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid regex: {}", e))?,
        );
    }
    let conn = open_index_db_for_search()?;
    let fts_ready = index_search_is_ready(&conn);
    let covering_root = covering_index_root(&conn, &root_key)?;
    let refresh_root = covering_root.as_deref().unwrap_or(&root_key);
    let clean_index = watch_state_covers_root(&conn, &root_key)?;
    if !clean_index {
        spawn_index_refresh(refresh_root, opts, covering_root.is_none());
    }
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let base_name_expr = "lower(s.value)";
    let full_path_expr =
        "lower((CASE WHEN d.path = '/' THEN '/' || s.value ELSE d.path || '/' || s.value END) || CASE WHEN e.kind = 1 THEN '/' ELSE '' END)";
    let sql_field_expr = if opts.force_full {
        full_path_expr
    } else {
        base_name_expr
    };
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let mut sql = String::from(
        "SELECT d.path, s.value, e.kind, e.mtime, e.size, e.activity
         FROM entries e
         JOIN dirs d ON e.dir_id = d.id
         JOIN strings s ON e.name_id = s.id
         WHERE (d.path = ? OR (d.path >= ? AND d.path < ?))",
    );
    let mut sql_params = vec![root_key.clone(), root_prefix.clone(), root_prefix_end];
    match type_flag {
        Some(TypeFlag::Dir) => sql.push_str(" AND e.kind = 1"),
        Some(TypeFlag::File) => sql.push_str(" AND e.kind != 1"),
        None => {}
    }
    for term in &terms {
        if let Some((condition, extra_params)) = sql_prefilter_for_term(
            term,
            opts.regex_mode,
            opts.force_full,
            sql_field_expr,
            fts_ready,
        ) {
            sql.push_str(" AND ");
            sql.push('(');
            sql.push_str(&condition);
            sql.push(')');
            sql_params.extend(extra_params);
        }
    }
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params_from_iter(sql_params.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    if can_stream_direct(opts, use_style) {
        let stdout = io::stdout();
        let mut lock = BufWriter::with_capacity(64 * 1024, stdout.lock());
        let mut cache_state = if opts.cache_output {
            init_raw_cache_state()
        } else {
            None
        };
        let mut written_since_flush = 0usize;
        let mut emitted = 0usize;

        for row in rows {
            let (dir_path, name, kind, _mtime, _size, _activity) =
                row.map_err(|e| e.to_string())?;
            let is_dir = kind == 1;
            if matches!(type_flag, Some(TypeFlag::Dir)) && !is_dir {
                continue;
            }
            if matches!(type_flag, Some(TypeFlag::File)) && is_dir {
                continue;
            }
            let mut path = if dir_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", dir_path, name)
            };
            if is_dir {
                path.push('/');
            }
            if is_root_index_excluded_path(&root_key, &path) {
                continue;
            }
            if opts.visible_only && path_has_hidden_component_below(&path, &root_key) {
                continue;
            }
            let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            let matched = if opts.force_full {
                regexes.iter().all(|re| re.is_match(&path))
            } else {
                regexes.iter().all(|re| re.is_match(base))
            };
            if !matched {
                continue;
            }
            if opts.force_full && regexes.len() > 1 && !regexes.iter().any(|re| re.is_match(base)) {
                continue;
            }
            if opts.limit.is_some_and(|limit| emitted >= limit) {
                break;
            }
            if let Some(state) = cache_state.as_mut() {
                cache_raw_record_path(&path, is_dir, true, state);
            }
            if opts.index_binary {
                write_binary_path_record(&mut lock, &path, true).map_err(|e| e.to_string())?;
            } else {
                let display_path = escape_terminal_text(&path);
                lock.write_all(display_path.as_bytes())
                    .and_then(|_| lock.write_all(b"\n"))
                    .map_err(|e| e.to_string())?;
            }
            emitted += 1;
            written_since_flush += 1;
            if written_since_flush >= INDEX_STREAM_FLUSH_LINES {
                lock.flush().map_err(|e| e.to_string())?;
                written_since_flush = 0;
            }
        }
        lock.flush().map_err(|e| e.to_string())?;
        if let Some(mut state) = cache_state {
            let _ = state.cache.flush();
        }
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: false,
        });
    }

    let mut results = Vec::new();
    for row in rows {
        let (dir_path, name, kind, mtime, size, activity) = row.map_err(|e| e.to_string())?;
        let is_dir = kind == 1;
        let is_symlink = kind == 2;
        if matches!(type_flag, Some(TypeFlag::Dir)) && !is_dir {
            continue;
        }
        if matches!(type_flag, Some(TypeFlag::File)) && is_dir {
            continue;
        }
        let mut path = if dir_path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", dir_path, name)
        };
        if is_dir {
            path.push('/');
        }
        if is_root_index_excluded_path(&root_key, &path) {
            continue;
        }
        if opts.visible_only && path_has_hidden_component_below(&path, &root_key) {
            continue;
        }
        let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let matched = if opts.force_full {
            regexes.iter().all(|re| re.is_match(&path))
        } else {
            regexes.iter().all(|re| re.is_match(base))
        };
        if !matched {
            continue;
        }
        if opts.force_full && regexes.len() > 1 && !regexes.iter().any(|re| re.is_match(base)) {
            continue;
        }
        results.push(SearchResult {
            path,
            path_encoded: true,
            is_dir,
            is_symlink,
            metadata: None,
            indexed_activity_nanos: activity.or(mtime),
            indexed_size: size.and_then(|value| u64::try_from(value).ok()),
        });
    }
    results.sort_by(|a, b| a.path.cmp(&b.path));
    if clean_index {
        let _ = populate_indexed_dirsize_cache(&conn, &root_key, &results, opts, cache);
    }
    Ok(SearchRun {
        lines: final_transform(results, opts, use_style, stdout_is_tty, colors, cache, None),
        timed_out: false,
    })
}

pub(crate) fn clean_watcher_covers_search(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
) -> Result<bool, String> {
    let (root_raw, _) = indexed_root_from_opts(opts, content_spec)?;
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let conn = open_index_db_for_search()?;
    watch_state_covers_root(&conn, &root_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn binary_index_record_restores_non_utf8_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let raw = PathBuf::from(OsString::from_vec(b"/tmp/invalid-\xff".to_vec()));
        let encoded = fsx::encode_lossless_path(&raw);
        let mut output = Vec::new();
        write_binary_path_record(&mut output, &encoded, true).expect("write record");

        let length = u32::from_le_bytes(output[..4].try_into().expect("length")) as usize;
        assert_eq!(length, raw.as_os_str().as_bytes().len());
        assert_eq!(&output[4..], raw.as_os_str().as_bytes());
    }
}
