use crate::model::EntryInfo;
use clap::{Parser, ValueEnum};
use lscolors::LsColors;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(ValueEnum, Clone, Debug, Copy, PartialEq)]
pub(crate) enum SortBy {
    Name,
    Type,
    #[value(name = "time", alias = "date")]
    Date,
    Size,
    #[value(name = "dircount")]
    DirCount,
    #[value(name = "filecount")]
    FileCount,
}

#[derive(ValueEnum, Clone, Debug, Copy, PartialEq, Eq)]
pub(crate) enum OutputWhen {
    Always,
    Auto,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetailColumn {
    Perms,
    SizeLogical,
    DirCount,
    FileCount,
    Owner,
    Time,
    Group,
    SizeTrue,
    Git,
}

#[derive(Parser, Clone)]
#[command(name = "twig")]
#[command(version)]
#[command(
    about = "A faster, more functional, more fine grained, more comprehensive, more user friendly and more modular eza clone",
    long_about = None
)]
pub(crate) struct Cli {
    /// List all files, including hidden ones
    #[arg(short, long)]
    pub(crate) all: bool,

    /// List all files, but exclude . and ..
    #[arg(short = 'A', long)]
    pub(crate) almost_all: bool,

    /// Use a long listing format (short for -Lptos --show-targets)
    #[arg(short, long)]
    pub(crate) long: bool,

    /// List one entry per line
    #[arg(short = 'L', long = "list")]
    pub(crate) list: bool,

    /// Only show directories in the listed folder
    #[arg(short = 'd', long = "dirs-only")]
    pub(crate) dirs_only: bool,

    /// List the directory itself, not its contents
    #[arg(short = 'n', long = "no-traverse")]
    pub(crate) no_traverse: bool,

    /// Show permissions
    #[arg(short, long)]
    pub(crate) permissions: bool,

    /// Show size: files use logical bytes; dirs use allocated blocks
    #[arg(short, long)]
    pub(crate) size: bool,

    /// Show recursive directory and file counts for directories; auto-sorts by total files + dirs ascending
    #[arg(short = 'c', long = "counts")]
    pub(crate) counts: bool,

    /// Show owner user
    #[arg(short, long)]
    pub(crate) owner: bool,

    /// Show group
    #[arg(short, long)]
    pub(crate) group: bool,

    /// Show modification time
    #[arg(short = 't', long = "modified")]
    pub(crate) modified: bool,

    /// Sort by modification date ascending
    #[arg(short = 'T', visible_short_alias = 'D', long = "date-sort")]
    pub(crate) date_sort: bool,

    /// Append indicator (one of /=>@|) to entries
    #[arg(short = 'F', long)]
    pub(crate) classify: bool,

    /// Which field to sort by
    #[arg(long, value_enum, default_value = "type")]
    pub(crate) sort: SortBy,

    /// Reverse output order
    #[arg(short, long)]
    pub(crate) reverse: bool,

    /// Control colorized output
    #[arg(long, value_enum, default_value = "auto")]
    pub(crate) color: OutputWhen,

    /// Render names as terminal hyperlinks (default: never; plain -U means auto)
    #[arg(
        short = 'U',
        long,
        value_enum,
        default_value = "never",
        default_missing_value = "auto",
        num_args = 0..=1,
        require_equals = true
    )]
    pub(crate) hyperlink: OutputWhen,

    /// Show symlink targets
    #[arg(short = 'x', long = "show-targets")]
    pub(crate) show_targets: bool,

    /// Show absolute paths in output
    #[arg(short = 'X', visible_short_alias = 'b', long = "absolute")]
    pub(crate) absolute: bool,

    /// Dereference symlink targets for size/time calculations
    #[arg(short = '1', long = "dereference")]
    pub(crate) dereference: bool,

    /// Show Git columns: file status in repos and repo-root status for listed repo dirs
    #[arg(short = 'G', long)]
    pub(crate) git: bool,

    /// Fetch remotes for all Git repo roots in the listed directory before rendering
    #[arg(short = 'f', long = "git-fetch")]
    pub(crate) git_fetch: bool,

    /// Show true size: files use allocated blocks; dirs use recursive allocated blocks including hidden descendants (auto-sorts by size ascending)
    #[arg(short = 'S', long = "true-size")]
    pub(crate) true_size: bool,

    /// Disable hardlink deduplication when calculating true sizes
    #[arg(
        short = 'H',
        long = "no-dedupe-hardlinks",
        action = clap::ArgAction::SetFalse,
        default_value_t = true,
        requires = "true_size"
    )]
    pub(crate) dedupe_hardlinks: bool,

    /// Cache shown output paths to /tmp/fzf-history-$USER/universal-last-{dirs,files}-<fish pid>
    #[arg(long)]
    pub(crate) cache_raw: bool,

    /// Show a header row for list/detailed output
    #[arg(short = 'v', long)]
    pub(crate) header: bool,

    /// Internal path currently being rendered
    #[arg(skip)]
    pub(crate) path: PathBuf,

    /// Paths to list
    #[arg(value_name = "PATH", default_value = ".")]
    pub(crate) paths: Vec<PathBuf>,
}

pub(crate) struct Context {
    pub(crate) lscolors: LsColors,
    pub(crate) color_enabled: bool,
    pub(crate) classify: bool,
    pub(crate) show_perms: bool,
    pub(crate) show_size_logical: bool,
    pub(crate) show_size_true: bool,
    pub(crate) replace_size_with_true: bool,
    pub(crate) show_counts: bool,
    pub(crate) show_owner: bool,
    pub(crate) show_group: bool,
    pub(crate) show_time: bool,
    pub(crate) hyperlink: bool,
    pub(crate) show_targets: bool,
    pub(crate) absolute: bool,
    pub(crate) dereference: bool,
    pub(crate) show_git: bool,
    pub(crate) show_git_repos: bool,
    pub(crate) show_git_remote: bool,
    pub(crate) dedupe_hardlinks: bool,
    pub(crate) reverse: bool,
    pub(crate) header: bool,
    pub(crate) column_preference: Vec<DetailColumn>,
    pub(crate) sort_by: SortBy,
    pub(crate) sort_counts_total: bool,
    pub(crate) sort_reverse: bool,
    pub(crate) cwd: PathBuf,
}

fn push_unique_column(columns: &mut Vec<DetailColumn>, column: DetailColumn) {
    if !columns.contains(&column) {
        columns.push(column);
    }
}

fn push_long_shorthand_columns(columns: &mut Vec<DetailColumn>) {
    push_unique_column(columns, DetailColumn::Perms);
    push_unique_column(columns, DetailColumn::SizeLogical);
    push_unique_column(columns, DetailColumn::Owner);
    push_unique_column(columns, DetailColumn::Time);
}

pub(crate) fn dot_entry_rank(name: &str) -> Option<u8> {
    match name {
        "." => Some(0),
        ".." => Some(1),
        _ => None,
    }
}

pub(crate) fn pin_dot_entries_top(entries: &mut Vec<EntryInfo>) {
    if entries.is_empty() {
        return;
    }

    let mut pinned: Vec<(u8, EntryInfo)> = Vec::new();
    let mut rest: Vec<EntryInfo> = Vec::with_capacity(entries.len());

    for entry in entries.drain(..) {
        if let Some(rank) = dot_entry_rank(&entry.display_name) {
            pinned.push((rank, entry));
        } else {
            rest.push(entry);
        }
    }

    pinned.sort_by_key(|(rank, _)| *rank);
    entries.extend(pinned.into_iter().map(|(_, entry)| entry));
    entries.extend(rest);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImplicitSort {
    Counts,
    TrueSize,
    Date,
}

#[derive(Clone, Debug, Default)]
struct ParsedArgState {
    columns: Vec<DetailColumn>,
    sort_explicit: bool,
    implicit_sort: Option<ImplicitSort>,
}

fn scan_argument_state() -> ParsedArgState {
    let mut state = ParsedArgState::default();
    let mut stop_parsing_flags = false;
    let mut expect_sort_value = false;

    for arg in std::env::args_os().skip(1) {
        let arg = arg.to_string_lossy();
        if expect_sort_value {
            state.sort_explicit = true;
            expect_sort_value = false;
            continue;
        }
        if stop_parsing_flags {
            continue;
        }
        if arg == "--" {
            stop_parsing_flags = true;
            continue;
        }
        if arg == "--sort" {
            state.sort_explicit = true;
            expect_sort_value = true;
            continue;
        }
        if arg.starts_with("--sort=") {
            state.sort_explicit = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "long" => push_long_shorthand_columns(&mut state.columns),
                "permissions" => push_unique_column(&mut state.columns, DetailColumn::Perms),
                "size" => push_unique_column(&mut state.columns, DetailColumn::SizeLogical),
                "counts" => {
                    push_unique_column(&mut state.columns, DetailColumn::DirCount);
                    push_unique_column(&mut state.columns, DetailColumn::FileCount);
                    state.implicit_sort.get_or_insert(ImplicitSort::Counts);
                }
                "owner" => push_unique_column(&mut state.columns, DetailColumn::Owner),
                "modified" => push_unique_column(&mut state.columns, DetailColumn::Time),
                "group" => push_unique_column(&mut state.columns, DetailColumn::Group),
                "true-size" => {
                    push_unique_column(&mut state.columns, DetailColumn::SizeTrue);
                    state.implicit_sort.get_or_insert(ImplicitSort::TrueSize);
                }
                "date-sort" => {
                    state.implicit_sort.get_or_insert(ImplicitSort::Date);
                }
                "git" => push_unique_column(&mut state.columns, DetailColumn::Git),
                _ => {}
            }
            continue;
        }
        if let Some(shorts) = arg.strip_prefix('-') {
            if shorts.is_empty() {
                continue;
            }
            for ch in shorts.chars() {
                match ch {
                    'l' => push_long_shorthand_columns(&mut state.columns),
                    'p' => push_unique_column(&mut state.columns, DetailColumn::Perms),
                    's' => push_unique_column(&mut state.columns, DetailColumn::SizeLogical),
                    'c' => {
                        push_unique_column(&mut state.columns, DetailColumn::DirCount);
                        push_unique_column(&mut state.columns, DetailColumn::FileCount);
                        state.implicit_sort.get_or_insert(ImplicitSort::Counts);
                    }
                    'o' => push_unique_column(&mut state.columns, DetailColumn::Owner),
                    't' => push_unique_column(&mut state.columns, DetailColumn::Time),
                    'g' => push_unique_column(&mut state.columns, DetailColumn::Group),
                    'S' => {
                        push_unique_column(&mut state.columns, DetailColumn::SizeTrue);
                        state.implicit_sort.get_or_insert(ImplicitSort::TrueSize);
                    }
                    'G' => push_unique_column(&mut state.columns, DetailColumn::Git),
                    'T' | 'D' => {
                        state.implicit_sort.get_or_insert(ImplicitSort::Date);
                    }
                    _ => {}
                }
            }
        }
    }

    state
}

fn parsed_argument_state() -> ParsedArgState {
    static STATE: OnceLock<ParsedArgState> = OnceLock::new();
    STATE.get_or_init(scan_argument_state).clone()
}

fn is_detail_column_enabled(column: DetailColumn, ctx: &Context) -> bool {
    match column {
        DetailColumn::Perms => ctx.show_perms,
        DetailColumn::SizeLogical => ctx.show_size_logical,
        DetailColumn::DirCount | DetailColumn::FileCount => ctx.show_counts,
        DetailColumn::Owner => ctx.show_owner,
        DetailColumn::Time => ctx.show_time,
        DetailColumn::Group => ctx.show_group,
        DetailColumn::SizeTrue => ctx.show_size_true,
        DetailColumn::Git => ctx.show_git || ctx.show_git_repos || ctx.show_git_remote,
    }
}

pub(crate) fn build_detail_columns(ctx: &Context) -> Vec<DetailColumn> {
    let mut columns = Vec::new();
    for column in &ctx.column_preference {
        if ctx.replace_size_with_true && *column == DetailColumn::SizeTrue {
            // In long+true-size replacement mode, keep TSIZE only in the
            // original SIZE slot instead of honoring standalone -S position.
            continue;
        }
        let effective = if ctx.replace_size_with_true && *column == DetailColumn::SizeLogical {
            DetailColumn::SizeTrue
        } else {
            *column
        };
        if is_detail_column_enabled(effective, ctx) {
            push_unique_column(&mut columns, effective);
        }
    }
    for column in [
        DetailColumn::Perms,
        DetailColumn::SizeLogical,
        DetailColumn::DirCount,
        DetailColumn::FileCount,
        DetailColumn::Owner,
        DetailColumn::Time,
        DetailColumn::Group,
        DetailColumn::SizeTrue,
        DetailColumn::Git,
    ] {
        if is_detail_column_enabled(column, ctx) {
            push_unique_column(&mut columns, column);
        }
    }
    columns
}

pub(crate) fn build_context_and_sort_state(cli: &Cli) -> (Context, bool, bool, bool, bool) {
    let replace_logical_size = cli.long && cli.true_size;
    let parsed_args = parsed_argument_state();
    let sort_explicit = parsed_args.sort_explicit;
    let implicit_sort = if sort_explicit {
        None
    } else {
        parsed_args.implicit_sort
    };
    let effective_sort = match implicit_sort {
        Some(ImplicitSort::Counts) => SortBy::DirCount,
        Some(ImplicitSort::TrueSize) => SortBy::Size,
        Some(ImplicitSort::Date) => SortBy::Date,
        None => {
            if cli.no_traverse && !sort_explicit {
                SortBy::Date
            } else {
                cli.sort
            }
        }
    };
    let implicit_ascending_sort = implicit_sort.is_some();
    let sort_counts_total = matches!(implicit_sort, Some(ImplicitSort::Counts));
    let pin_dot_entries = !sort_explicit && implicit_sort.is_none();
    let piped_output = !io::stdout().is_terminal();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let color_enabled = output_enabled(cli.color, piped_output, false);
    let classify_enabled = cli.classify && !piped_output;
    let hyperlink_enabled = output_enabled(cli.hyperlink, piped_output, false);
    let lscolors = LsColors::from_env().unwrap_or_default();

    let ctx = Context {
        lscolors,
        color_enabled,
        classify: classify_enabled,
        show_perms: cli.permissions || cli.long,
        show_size_logical: (cli.size || cli.long) && !replace_logical_size,
        show_size_true: cli.true_size,
        replace_size_with_true: replace_logical_size,
        show_counts: cli.counts,
        show_owner: cli.owner || cli.long,
        show_group: cli.group,
        show_time: cli.modified || cli.long,
        hyperlink: hyperlink_enabled,
        show_targets: cli.show_targets || cli.long,
        absolute: cli.absolute,
        dereference: cli.dereference,
        show_git: false,
        show_git_repos: false,
        show_git_remote: false,
        dedupe_hardlinks: cli.dedupe_hardlinks,
        reverse: cli.reverse,
        header: cli.header,
        column_preference: parsed_args.columns,
        sort_by: effective_sort,
        sort_counts_total,
        sort_reverse: cli.reverse ^ implicit_ascending_sort,
        cwd,
    };
    (
        ctx,
        sort_explicit,
        implicit_ascending_sort,
        pin_dot_entries,
        piped_output,
    )
}

pub(crate) fn output_enabled(mode: OutputWhen, piped_output: bool, over_auto_limit: bool) -> bool {
    match mode {
        OutputWhen::Always => true,
        OutputWhen::Auto => !piped_output && !over_auto_limit,
        OutputWhen::Never => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_time_accepts_date_alias() {
        let time = Cli::try_parse_from(["twig", "--sort", "time"]).unwrap();
        let date = Cli::try_parse_from(["twig", "--sort", "date"]).unwrap();

        assert_eq!(time.sort, SortBy::Date);
        assert_eq!(date.sort, SortBy::Date);
    }

    #[test]
    fn directory_modes_remain_distinct() {
        let dirs_only = Cli::try_parse_from(["twig", "-d"]).unwrap();
        let no_traverse = Cli::try_parse_from(["twig", "-n"]).unwrap();

        assert!(dirs_only.dirs_only);
        assert!(!dirs_only.no_traverse);
        assert!(no_traverse.no_traverse);
        assert!(!no_traverse.dirs_only);
    }
}
