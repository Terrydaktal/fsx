//! Terminal output, preview rendering, and progress estimation.

mod eta;
mod preview;
mod progress;
mod summary;

pub(crate) use eta::TransferEtaEstimator;
pub(crate) use preview::{
    build_change_tree, collect_source_top_entries, print_changed_top_preview,
    print_changed_top_preview_with_cache, remap_item_under_prefix, remap_path_set_under_prefix,
    render_showall_tree_to_string_with_cache,
};
pub(crate) use progress::{
    finish_progress_render_state, format_bytes_binary, print_copy_duration_summary,
    print_summary_rate_line, print_transfer_columns_header, print_transfer_progress_bars,
    reset_progress_render_state,
};
pub(crate) use summary::{format_number, print_counts_table, print_preview_counts_table};

pub(crate) const OKBLUE: &str = "\x1b[94m";
pub(crate) const LIGHT_TEAL: &str = "\x1b[96m";
pub(crate) const OKGREEN: &str = "\x1b[92m";
pub(crate) const WARNING: &str = "\x1b[93m";
pub(crate) const FAIL: &str = "\x1b[91m";
pub(crate) const WHITE: &str = "\x1b[97m";
pub(crate) const DIM: &str = "\x1b[90m";
pub(crate) const ENDC: &str = "\x1b[0m";

use crate::domain::{LogLevel, TransferMode};

pub(crate) fn log(mode: TransferMode, msg: &str, level: LogLevel) {
    finish_progress_render_state();
    match level {
        LogLevel::Error => eprintln!("{FAIL}ERROR: {msg}{ENDC}"),
        LogLevel::Warn => eprintln!("{WARNING}WARNING: {msg}{ENDC}"),
        LogLevel::Info => println!("{OKBLUE}{}: {msg}{ENDC}", mode.word()),
    }
}

pub(crate) fn log_transfer_complete(mode: TransferMode) {
    if matches!(mode, TransferMode::Copy) {
        println!();
    }
    log(
        mode,
        &format!("{} complete.", mode.word_cap()),
        LogLevel::Info,
    );
    if matches!(mode, TransferMode::Copy) {
        println!();
    }
}

pub(crate) fn fmt_mode_word(label: &str, active: bool) -> String {
    if active {
        format!("{OKGREEN}{label}{ENDC}")
    } else {
        format!("{DIM}{label}{ENDC}")
    }
}

pub(crate) fn print_preview_root_line(
    preview_root: &std::path::Path,
    highlight_new_leaf: bool,
    emphasize_non_new: bool,
) {
    let full = preview_root.display().to_string();
    if !highlight_new_leaf {
        if emphasize_non_new {
            println!("{WARNING}{}{ENDC}", full);
        } else {
            println!("{full}");
        }
        return;
    }

    let trimmed = full.trim_end_matches('/');
    let p = std::path::Path::new(trimmed);
    let leaf = match p.file_name() {
        Some(v) => v.to_string_lossy().to_string(),
        None => {
            println!("{WARNING}{}{ENDC}", full);
            return;
        }
    };
    if leaf.is_empty() {
        println!("{WARNING}{}{ENDC}", full);
        return;
    }

    let parent = p
        .parent()
        .map(|x| x.display().to_string())
        .unwrap_or_default();
    let parent_trimmed = parent.trim_end_matches('/');
    if parent_trimmed.is_empty() {
        if p.is_absolute() {
            println!("{WARNING}/{OKGREEN}{leaf}/{ENDC}");
        } else {
            println!("{OKGREEN}{leaf}/{ENDC}");
        }
    } else {
        println!("{WARNING}{parent_trimmed}/{OKGREEN}{leaf}/{ENDC}");
    }
}
