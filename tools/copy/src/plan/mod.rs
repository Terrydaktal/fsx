//! Transfer planning: path resolution, scanning, and collision policy.

mod policy;
mod resolve;
mod scanner;

pub(crate) use policy::{
    classify_file_relation, parse_merge_collision_policy, regular_file_collision_change,
    sync_regular_file_change,
};
pub(crate) use resolve::{
    can_fast_rename_same_fs, create_destination_parents, destination_available_bytes,
    endpoint_to_rsync, enrich_remote_spec, existing_probe_path, parse_remote_spec,
    realpath_allow_missing, resolve_destination_for_dir, resolve_destination_for_file,
    resolve_source, to_real_path,
};
pub(crate) use scanner::{
    build_destination_index, count_tree_any, map_dir_dest, map_dir_dest_path, normalize_rel,
    pre_scan_directory, pre_scan_file, rel_matches_prefix, top_level_rel_component,
    DestinationKind,
};
