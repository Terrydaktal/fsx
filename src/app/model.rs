use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TypeFlag {
    File,
    Dir,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SortField {
    Date,
    Size,
    Name,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SortOrder {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug)]
pub(crate) struct NamePattern {
    pub(crate) type_flag: Option<TypeFlag>,
    pub(crate) regex: String,
}

#[derive(Clone, Debug)]
pub(crate) enum SearchDirMode {
    Path(String),
    Pattern(String),
}

#[derive(Clone, Debug)]
pub(crate) struct MountInfo {
    pub(crate) device: PathBuf,
    pub(crate) mount_point: PathBuf,
    pub(crate) fs_type: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub(crate) timeout_dur: Duration,
    pub(crate) timeout_explicit: bool,
    pub(crate) force_pattern_mode: bool,
    pub(crate) long_format: bool,
    pub(crate) long_extended: bool,
    pub(crate) sizes: bool,
    pub(crate) counts: bool,
    pub(crate) regex_mode: bool,
    pub(crate) sort_field: Option<SortField>,
    pub(crate) sort_order: Option<SortOrder>,
    pub(crate) limit: Option<usize>,
    pub(crate) reverse: bool,
    pub(crate) no_recurse: bool,
    pub(crate) follow_links: bool,
    pub(crate) respect_ignore: bool,
    pub(crate) visible_only: bool,
    pub(crate) threads_override: usize,
    pub(crate) threads_explicit: bool,
    pub(crate) cache_output: bool,
    pub(crate) snapshot_cache: bool,
    pub(crate) snapshot_refresh: bool,
    pub(crate) index_mode: bool,
    pub(crate) index_if_watched: bool,
    pub(crate) index_binary: bool,
    pub(crate) recent_limit: Option<usize>,
    pub(crate) index_refresh: Option<String>,
    pub(crate) index_snapshot: Option<String>,
    pub(crate) index_purge: Option<String>,
    pub(crate) watch: bool,
    pub(crate) watch_status: bool,
    pub(crate) watch_metrics: Option<String>,
    pub(crate) absolute_paths: bool,
    pub(crate) force_dir: bool,
    pub(crate) force_file: bool,
    pub(crate) force_full: bool,
    pub(crate) classify: bool,
    pub(crate) color_when: ColorWhen,
    pub(crate) hyperlinks: bool,
    pub(crate) highlight_match: bool,
    pub(crate) contains_all: bool,
    pub(crate) path_override: Option<String>,
    pub(crate) positional: Vec<String>,
}

pub(crate) struct SearchResult {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
    pub(crate) metadata: Option<fs::Metadata>,
    pub(crate) indexed_activity_nanos: Option<i64>,
    pub(crate) indexed_size: Option<u64>,
}

pub(crate) struct SearchRun {
    pub(crate) lines: Vec<String>,
    pub(crate) timed_out: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ContainsAllSpec {
    pub(crate) terms: Vec<String>,
    pub(crate) root: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct HighlightSpec {
    pub(crate) prefix_rules: Vec<Regex>,
    pub(crate) leaf_rules: Vec<Regex>,
}

#[derive(Clone, Debug)]
pub(crate) struct DirStats {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    pub(crate) human: String,
}

#[derive(Default)]
pub(crate) struct DirStatsCache {
    pub(crate) map: HashMap<String, DirStats>,
    pub(crate) bytes_map: HashMap<String, u64>,
}

pub(crate) struct RawCacheState {
    pub(crate) dirs: BufWriter<File>,
    pub(crate) files: BufWriter<File>,
    pub(crate) seen_dirs: HashSet<String>,
    pub(crate) seen_files: HashSet<String>,
}

#[derive(Clone)]
pub(crate) struct ColorSpec {
    pub(crate) by_key: HashMap<String, String>,
    pub(crate) suffix_globs: HashMap<String, (usize, String)>,
    pub(crate) globs: Vec<(usize, Regex, String)>,
    pub(crate) color_prefix_dir: String,
    pub(crate) color_dir: String,
    pub(crate) color_link: String,
    pub(crate) color_exec: String,
}

pub(crate) fn system_time_to_unix_nanos(value: SystemTime) -> Option<i64> {
    let duration = value.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_nanos()).ok()
}
