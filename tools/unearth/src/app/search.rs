use super::filesystem::{root_prefers_single_thread, walk_fast, walk_rayon_worker, PathInfo};
use super::model::{
    ColorSpec, ContainsAllSpec, DirStatsCache, Options, SearchDirMode, SearchResult, SearchRun,
    TypeFlag,
};
use super::patterns::{
    expand_home_path, parse_name_pattern, parse_search_dir, pattern_prefers_full_path,
    term_selectivity_score,
};
use super::presentation::{
    cache_raw_record_path, can_stream_direct, compile_highlight_spec, escape_terminal_text,
    final_transform, init_raw_cache_state, render_styled_path, style_enabled, RenderCache,
    RenderContext,
};
use crossbeam_channel::{bounded, unbounded, Sender};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct TimeoutGuard {
    cancel: Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TimeoutGuard {
    fn new(duration: std::time::Duration, flag: Arc<AtomicBool>) -> Self {
        let (cancel, receiver) = bounded(0);
        let join = std::thread::spawn(move || {
            if receiver.recv_timeout(duration).is_err() {
                flag.store(true, Ordering::Relaxed);
            }
        });
        Self {
            cancel,
            join: Some(join),
        }
    }
}

impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub(crate) fn run_standard(
    opts: &Options,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let name = parse_name_pattern(&opts.positional[0], opts.regex_mode);
    let mut type_flag = name.type_flag;
    if opts.force_dir {
        type_flag = Some(TypeFlag::Dir);
    }
    if opts.force_file {
        type_flag = Some(TypeFlag::File);
    }
    let stream_direct = can_stream_direct(opts, use_style);
    let re = RegexBuilder::new(&name.regex)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("Invalid regex: {}", e))?;
    let is_catch_all = name.regex == ".*" || name.regex == "^.*$";
    let timeout_triggered = Arc::new(AtomicBool::new(false));
    let _timeout_guard = TimeoutGuard::new(opts.timeout_dur, timeout_triggered.clone());

    if stream_direct {
        let (tx, rx) = unbounded::<Vec<PathInfo>>();
        let opts_clone = opts.clone();
        let timeout_fast = timeout_triggered.clone();
        if opts.positional.len() == 1 {
            rayon::spawn(move || {
                walk_fast(
                    PathBuf::from("."),
                    &re,
                    is_catch_all,
                    &tx,
                    opts_clone.visible_only,
                    opts_clone.respect_ignore,
                    opts_clone.no_recurse,
                    opts_clone.follow_links,
                    type_flag,
                    false,
                    false,
                    &timeout_fast,
                )
            });
        } else {
            let p_raw = &opts.positional[1];
            let sd = parse_search_dir(p_raw, opts.regex_mode, opts.force_pattern_mode);
            let sd_re = match &sd {
                SearchDirMode::Pattern(pattern) => Some(
                    RegexBuilder::new(pattern)
                        .case_insensitive(true)
                        .build()
                        .map_err(|e| format!("Invalid search-directory regex: {}", e))?,
                ),
                SearchDirMode::Path(_) => None,
            };
            rayon::spawn(move || match sd {
                SearchDirMode::Path(p) => {
                    let root_serial = root_prefers_single_thread(Path::new(&p));
                    walk_fast(
                        PathBuf::from(p),
                        &re,
                        is_catch_all,
                        &tx,
                        opts_clone.visible_only,
                        opts_clone.respect_ignore,
                        opts_clone.no_recurse,
                        opts_clone.follow_links,
                        type_flag,
                        false,
                        root_serial,
                        &timeout_fast,
                    )
                }
                SearchDirMode::Pattern(_) => {
                    let mut roots = Vec::new();
                    let (rtx, rrx) = unbounded::<Vec<PathInfo>>();
                    let sd_re = sd_re.expect("pattern regex was compiled before spawning");
                    walk_fast(
                        PathBuf::from("/"),
                        &sd_re,
                        false,
                        &rtx,
                        opts_clone.visible_only,
                        opts_clone.respect_ignore,
                        false,
                        opts_clone.follow_links,
                        Some(TypeFlag::Dir),
                        false,
                        false,
                        &timeout_fast,
                    );
                    drop(rtx);
                    for chunk in rrx {
                        for info in chunk {
                            roots.push(info.path);
                        }
                    }
                    roots.into_par_iter().for_each_with(tx.clone(), |tx_c, d| {
                        let next_serial = root_prefers_single_thread(&d);
                        walk_fast(
                            d,
                            &re,
                            is_catch_all,
                            tx_c,
                            opts_clone.visible_only,
                            opts_clone.respect_ignore,
                            opts_clone.no_recurse,
                            opts_clone.follow_links,
                            type_flag,
                            false,
                            next_serial,
                            &timeout_fast,
                        );
                    });
                }
            });
        }

        let stdout = io::stdout();
        let mut lock = BufWriter::with_capacity(128 * 1024, stdout.lock());
        let mut cache_state = if opts.cache_output {
            init_raw_cache_state()
        } else {
            None
        };
        let mut emitted = 0usize;
        let mut stopped_by_limit = false;

        for chunk in rx {
            for info in chunk {
                if opts.limit.is_some_and(|limit| emitted >= limit) {
                    continue;
                }
                if let Some(state) = cache_state.as_mut() {
                    cache_raw_record_path(&info.path.to_string_lossy(), info.is_dir, state);
                }
                let raw_path = info.path.to_string_lossy();
                let display_path = escape_terminal_text(&raw_path);
                lock.write_all(display_path.as_bytes())
                    .map_err(|e| e.to_string())?;
                if info.is_dir && !info.path.as_os_str().as_bytes().ends_with(b"/") {
                    lock.write_all(b"/").map_err(|e| e.to_string())?;
                }
                lock.write_all(b"\n").map_err(|e| e.to_string())?;
                emitted += 1;
                if opts.limit.is_some_and(|limit| emitted >= limit) {
                    timeout_triggered.store(true, Ordering::Relaxed);
                    stopped_by_limit = true;
                }
            }
        }

        if let Some(mut state) = cache_state {
            let _ = state.dirs.flush();
            let _ = state.files.flush();
        }
        lock.flush().map_err(|e| e.to_string())?;
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: timeout_triggered.load(Ordering::Relaxed) && !stopped_by_limit,
        });
    }

    let mut results = Vec::new();
    let needs_metadata = opts.long_format || opts.sort_field.is_some() || opts.sizes;
    let (tx, rx) = unbounded::<Vec<SearchResult>>();
    let opts_clone = opts.clone();
    if opts.positional.len() == 1 {
        let timeout_walk = timeout_triggered.clone();
        rayon::spawn(move || {
            walk_rayon_worker(
                PathBuf::from("."),
                &re,
                is_catch_all,
                &tx,
                &opts_clone,
                type_flag,
                false,
                false,
                needs_metadata,
                false,
                &timeout_walk,
            )
        });
    } else {
        let p_raw = &opts.positional[1];
        match parse_search_dir(p_raw, opts.regex_mode, opts.force_pattern_mode) {
            SearchDirMode::Path(p) => {
                let timeout_walk = timeout_triggered.clone();
                rayon::spawn(move || {
                    let root_serial = root_prefers_single_thread(Path::new(&p));
                    walk_rayon_worker(
                        PathBuf::from(p),
                        &re,
                        is_catch_all,
                        &tx,
                        &opts_clone,
                        type_flag,
                        false,
                        false,
                        needs_metadata,
                        root_serial,
                        &timeout_walk,
                    )
                })
            }
            SearchDirMode::Pattern(sd_rx) => {
                let sd_re = RegexBuilder::new(&sd_rx)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| format!("Invalid search-directory regex: {}", e))?;
                let timeout_walk = timeout_triggered.clone();
                rayon::spawn(move || {
                    let mut roots = Vec::new();
                    let (rtx, rrx) = unbounded::<Vec<SearchResult>>();
                    walk_rayon_worker(
                        PathBuf::from("/"),
                        &sd_re,
                        false,
                        &rtx,
                        &opts_clone,
                        Some(TypeFlag::Dir),
                        false,
                        false,
                        false,
                        false,
                        &timeout_walk,
                    );
                    drop(rtx);
                    for chunk in rrx {
                        for r in chunk {
                            roots.push(PathBuf::from(r.path));
                        }
                    }
                    roots.into_par_iter().for_each_with(tx.clone(), |tx_c, d| {
                        let next_serial = root_prefers_single_thread(&d);
                        walk_rayon_worker(
                            d,
                            &re,
                            is_catch_all,
                            tx_c,
                            &opts_clone,
                            type_flag,
                            false,
                            false,
                            needs_metadata,
                            next_serial,
                            &timeout_walk,
                        );
                    });
                });
            }
        }
    }
    for chunk in rx {
        results.extend(chunk);
    }
    let highlight_spec = if opts.highlight_match {
        Some(compile_highlight_spec(&[(name.regex.clone(), false)])?)
    } else {
        None
    };
    Ok(SearchRun {
        lines: final_transform(
            results,
            opts,
            use_style,
            stdout_is_tty,
            colors,
            cache,
            highlight_spec.as_ref(),
        ),
        timed_out: timeout_triggered.load(Ordering::Relaxed),
    })
}

pub(crate) fn run_contains_all(
    opts: &Options,
    spec: ContainsAllSpec,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let timeout_triggered = Arc::new(AtomicBool::new(false));
    let _timeout_guard = TimeoutGuard::new(opts.timeout_dur, timeout_triggered.clone());

    let mut type_flag = if opts.force_dir {
        Some(TypeFlag::Dir)
    } else if opts.force_file {
        Some(TypeFlag::File)
    } else {
        None
    };
    let mut term_specs: Vec<(String, i64)> = Vec::new();
    for p in &spec.terms {
        let parsed = parse_name_pattern(p, opts.regex_mode);
        if parsed.type_flag == Some(TypeFlag::Dir) && !opts.force_file {
            type_flag = Some(TypeFlag::Dir);
        }
        term_specs.push((parsed.regex, term_selectivity_score(p, opts.regex_mode)));
    }
    term_specs.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)))
    });
    let regexes: Vec<String> = term_specs.into_iter().map(|(rx, _)| rx).collect();
    if regexes.is_empty() {
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: false,
        });
    }
    let compiled_regexes: Vec<Regex> = regexes
        .iter()
        .map(|pattern| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid regex: {}", e))
        })
        .collect::<Result<_, _>>()?;
    let first_re = compiled_regexes[0].clone();
    let is_catch_all = regexes[0] == ".*" || regexes[0] == "^.*$";
    let needs_metadata = opts.long_format || opts.sort_field.is_some() || opts.sizes;
    let (tx, rx) = unbounded::<Vec<SearchResult>>();
    let opts_clone = opts.clone();
    let root = spec.root.clone();
    let root_serial = root_prefers_single_thread(&root);
    let timeout_walk = timeout_triggered.clone();
    rayon::spawn(move || {
        walk_rayon_worker(
            root,
            &first_re,
            is_catch_all,
            &tx,
            &opts_clone,
            type_flag,
            opts_clone.force_full,
            false,
            needs_metadata,
            root_serial,
            &timeout_walk,
        )
    });

    let mut rows = Vec::new();
    for chunk in rx {
        for row in chunk {
            let base = row
                .path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("");
            let matches = if opts.force_full {
                compiled_regexes.iter().all(|re| re.is_match(&row.path))
                    && (regexes.len() == 1 || compiled_regexes.iter().any(|re| re.is_match(base)))
            } else {
                compiled_regexes.iter().all(|re| re.is_match(base))
            };
            if matches {
                rows.push(row);
            }
        }
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    let highlight_spec = if opts.highlight_match {
        let highlight_patterns: Vec<(String, bool)> = regexes
            .iter()
            .cloned()
            .map(|rx| (rx, opts.force_full))
            .collect();
        Some(compile_highlight_spec(&highlight_patterns)?)
    } else {
        None
    };

    Ok(SearchRun {
        lines: final_transform(
            rows,
            opts,
            use_style,
            stdout_is_tty,
            colors,
            cache,
            highlight_spec.as_ref(),
        ),
        timed_out: timeout_triggered.load(Ordering::Relaxed),
    })
}

pub(crate) fn run_full(
    opts: &Options,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let mut search_root = ".".to_string();
    let mut patterns = opts.positional.clone();
    if opts.positional.len() > 1 {
        if let Some(last) = opts.positional.last() {
            let expanded = expand_home_path(last);
            if Path::new(&expanded).is_dir() {
                search_root = expanded;
                patterns.pop();
            }
        }
    }
    let mut type_flag = if opts.force_dir {
        Some(TypeFlag::Dir)
    } else if opts.force_file {
        Some(TypeFlag::File)
    } else {
        None
    };
    let mut pattern_specs: Vec<(String, bool)> = Vec::new();
    for p in &patterns {
        let parsed = parse_name_pattern(p, opts.regex_mode);
        if parsed.type_flag == Some(TypeFlag::Dir) && !opts.force_file {
            type_flag = Some(TypeFlag::Dir);
        }
        pattern_specs.push((parsed.regex, pattern_prefers_full_path(p, opts.regex_mode)));
    }
    if pattern_specs.is_empty() {
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: false,
        });
    }
    let compiled_regexes: Vec<Regex> = pattern_specs
        .iter()
        .map(|(pattern, _)| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid regex: {}", e))
        })
        .collect::<Result<_, _>>()?;
    let re = compiled_regexes[0].clone();
    let timeout_triggered = Arc::new(AtomicBool::new(false));
    let _timeout_guard = TimeoutGuard::new(opts.timeout_dur, timeout_triggered.clone());
    let (tx, rx) = unbounded::<Vec<SearchResult>>();
    let opts_clone = opts.clone();
    let is_catch_all = pattern_specs[0].0 == ".*" || pattern_specs[0].0 == "^.*$";
    let first_full_path_match = pattern_specs[0].1;
    let prune_matched_dir_subtrees = false;
    let root_serial = root_prefers_single_thread(Path::new(&search_root));
    let timeout_walk = timeout_triggered.clone();
    rayon::spawn(move || {
        walk_rayon_worker(
            PathBuf::from(search_root),
            &re,
            is_catch_all,
            &tx,
            &opts_clone,
            type_flag,
            first_full_path_match,
            prune_matched_dir_subtrees,
            opts_clone.long_format
                || opts_clone.sort_field.is_some()
                || opts_clone.sizes
                || opts_clone.classify,
            root_serial,
            &timeout_walk,
        );
    });
    // Full-path searches with plain output do not need a result buffer. Stream
    // them directly so f/ff do not wait for the complete tree before printing.
    let stream_full = pattern_specs.len() == 1
        && !opts.counts
        && !opts.long_format
        && !opts.sizes
        && opts.sort_field.is_none()
        && !opts.reverse
        && !opts.snapshot_cache
        && !opts.snapshot_refresh
        && !opts.absolute_paths;
    if stream_full {
        let stdout = io::stdout();
        let mut output = BufWriter::with_capacity(128 * 1024, stdout.lock());
        let mut cache_state = if opts.cache_output {
            init_raw_cache_state()
        } else {
            None
        };
        let highlight_spec = if opts.highlight_match {
            Some(compile_highlight_spec(&pattern_specs)?)
        } else {
            None
        };
        let mut render_cache = RenderCache::default();
        let mut render_context = RenderContext {
            use_style,
            add_decorator: opts.classify,
            colors,
            opts,
            highlight: highlight_spec.as_ref(),
            cache: &mut render_cache,
        };
        let mut emitted = 0usize;
        let mut stopped_by_limit = false;
        for chunk in rx {
            for item in chunk {
                if opts.limit.is_some_and(|limit| emitted >= limit) {
                    continue;
                }
                if let Some(state) = cache_state.as_mut() {
                    cache_raw_record_path(&item.path, item.is_dir, state);
                }
                let display = render_styled_path(&item, &mut render_context);
                output
                    .write_all(display.as_bytes())
                    .and_then(|_| output.write_all(b"\n"))
                    .map_err(|e| e.to_string())?;
                emitted += 1;
                if opts.limit.is_some_and(|limit| emitted >= limit) {
                    timeout_triggered.store(true, Ordering::Relaxed);
                    stopped_by_limit = true;
                }
            }
        }
        if let Some(mut state) = cache_state {
            let _ = state.dirs.flush();
            let _ = state.files.flush();
        }
        output.flush().map_err(|e| e.to_string())?;
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: timeout_triggered.load(Ordering::Relaxed) && !stopped_by_limit,
        });
    }
    let mut rows = Vec::new();
    for chunk in rx {
        rows.extend(chunk);
    }
    for ((_, full_path_match), re_extra) in pattern_specs
        .iter()
        .skip(1)
        .zip(compiled_regexes.iter().skip(1))
    {
        rows.retain(|r| {
            if *full_path_match {
                re_extra.is_match(&r.path)
            } else {
                let base = r
                    .path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("");
                re_extra.is_match(base)
            }
        });
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    let highlight_spec = if opts.highlight_match {
        Some(compile_highlight_spec(&pattern_specs)?)
    } else {
        None
    };
    Ok(SearchRun {
        lines: final_transform(
            rows,
            opts,
            use_style,
            stdout_is_tty,
            colors,
            cache,
            highlight_spec.as_ref(),
        ),
        timed_out: timeout_triggered.load(Ordering::Relaxed),
    })
}
