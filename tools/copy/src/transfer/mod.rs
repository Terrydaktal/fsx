//! Transfer backends and operation lifecycle.

mod backup;
mod cleanup;
mod command;
mod copy_engine;
mod local;
mod orchestrator;
mod remote;
mod rename;
mod rsync;
mod telemetry;

pub(crate) use backup::{
    backup_base_path, backup_path_with_base, copy_path_to_backup, plan_backup_path,
};
pub(crate) use cleanup::remove_path_recursive;
pub(crate) use command::run_command_capture;
pub(crate) use copy_engine::preserve_directory_times_tree;
pub(crate) use local::run_rust_transfer;
pub(crate) use orchestrator::{
    flush_destination_writes, run_move_cleanup_phase, run_multi_source_file_batch,
    run_sync_cleanup_phase,
};
pub(crate) use remote::run_remote_transfer_mode;
pub(crate) use rename::premerge_fast_rename_noncolliding_children;
pub(crate) use rsync::run_rsync_transfer;
pub(crate) use telemetry::prefer_hdd_scheduler_for_paths;
