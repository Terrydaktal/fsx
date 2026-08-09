//! Shared filesystem primitives used by the fsx command-line tools.
//!
//! The crate reports filesystem facts. Consumers remain responsible for their
//! own CLI policy, sorting, presentation layout, search behavior, and transfer
//! decisions.

#[cfg(feature = "colors")]
pub mod colors;
#[cfg(feature = "scan")]
pub mod entry;
pub mod error;
pub mod format;
#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "ignore")]
pub mod ignore;
#[cfg(feature = "index")]
pub mod index;
pub mod metadata;
pub mod mount;
#[cfg(feature = "ntfs")]
pub mod ntfs;
pub mod overflow;
pub mod path;
pub mod path_cache;
#[cfg(feature = "scan")]
pub mod scan;
#[cfg(feature = "terminal")]
pub mod terminal;

#[cfg(feature = "scan")]
pub use entry::EntrySnapshot;
pub use error::{Error, Result};
pub use format::{
    format_count, format_size_compact, format_size_compact_3, format_size_iec, format_time_display,
};
pub use metadata::{
    EntryKind, HardlinkKey, MetadataSnapshot, allocated_size, is_hardlink_candidate, logical_size,
    metadata_snapshot, metadata_snapshot_for,
};
pub use path::{
    decode_lossless_path, encode_lossless_path, normalize_lexical, realpath_allow_missing,
    realpath_preserve_final_symlink,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[cfg(any(feature = "scan", feature = "ignore"))]
    use std::fs;

    #[cfg(any(feature = "scan", feature = "ignore"))]
    fn temp_root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fsx-{name}-{}", std::process::id()))
    }

    #[test]
    fn formats_shared_values() {
        assert_eq!(format_size_compact(4096), "4.0K");
        assert_eq!(format_count(1_532), "1,532");
        assert_eq!(format_size_iec(1024 * 1024), "1.00 MiB");
    }

    #[test]
    fn normalizes_paths_without_touching_the_filesystem() {
        assert_eq!(normalize_lexical(Path::new("a/./b/../c")), Path::new("a/c"));
        assert_eq!(normalize_lexical(Path::new("/a/../b")), Path::new("/b"));
        assert_eq!(
            normalize_lexical(Path::new("../../a")),
            Path::new("../../a")
        );
    }

    #[test]
    fn missing_relative_paths_are_anchored_to_the_working_directory() {
        let missing = realpath_allow_missing(Path::new("fsx-missing/a/b"));
        assert!(missing.is_absolute());
        assert!(missing.ends_with("fsx-missing/a/b"));
    }

    #[cfg(unix)]
    #[test]
    fn lossless_path_encoding_round_trips_invalid_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let raw = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/percent%/invalid\xff".to_vec(),
        ));
        let encoded = encode_lossless_path(&raw);
        assert_eq!(encoded, "/tmp/percent%25/invalid%FF");
        let decoded = decode_lossless_path(&encoded);
        assert_eq!(decoded.as_os_str().as_bytes(), raw.as_os_str().as_bytes());
    }

    #[cfg(feature = "scan")]
    #[test]
    fn scan_aggregates_and_retains_only_requested_depth() {
        let root = temp_root("scan");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("child")).expect("create tree");
        fs::write(root.join("root.txt"), b"root").expect("write root file");
        fs::write(root.join("child/nested.txt"), b"nested").expect("write nested file");

        let snapshot = scan::scan(&scan::ScanRequest {
            root: root.clone(),
            max_depth: None,
            retain_depth: Some(1),
            size_mode: scan::SizeMode::Logical,
            count_files: true,
            count_dirs: true,
            ..scan::ScanRequest::default()
        });
        assert!(snapshot.complete);
        assert_eq!(snapshot.entries.len(), 2);
        let aggregate = snapshot.aggregates.get(&root).expect("root aggregate");
        assert_eq!(aggregate.files, 2);
        assert_eq!(aggregate.dirs, 1);
        assert_eq!(aggregate.logical_size, 10);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "scan")]
    #[test]
    fn allocated_scan_includes_each_directory_inode_once() {
        let root = temp_root("allocated-scan");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("child")).expect("create tree");
        fs::write(root.join("child/file"), b"file").expect("write file");

        let snapshot = scan::scan(&scan::ScanRequest {
            root: root.clone(),
            size_mode: scan::SizeMode::Allocated,
            ..scan::ScanRequest::default()
        });
        let root_size = snapshot
            .aggregates
            .get(&root)
            .expect("root aggregate")
            .allocated_size;
        let child_size = snapshot
            .aggregates
            .get(&root.join("child"))
            .expect("child aggregate")
            .allocated_size;
        assert!(root_size >= child_size);
        let child_inode_size = metadata::allocated_size(
            &fs::symlink_metadata(root.join("child")).expect("child metadata"),
        );
        assert!(child_size >= child_inode_size);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(all(feature = "scan", unix))]
    #[test]
    fn follow_link_scan_stops_at_directory_cycles() {
        let root = temp_root("scan-cycle");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("child")).expect("create tree");
        std::os::unix::fs::symlink(&root, root.join("child/root-link")).expect("create cycle");
        let snapshot = scan::scan(&scan::ScanRequest {
            root: root.clone(),
            symlinks: scan::SymlinkMode::Follow,
            max_depth: Some(100),
            ..scan::ScanRequest::default()
        });
        assert!(snapshot.entries.len() < 10);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "ignore")]
    #[test]
    fn gitignore_matches_relative_paths_from_the_repo_root() {
        let root = temp_root("ignore");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ignored")).expect("create ignored directory");
        fs::write(root.join(".gitignore"), "ignored/\n*.tmp\n").expect("write ignore file");
        let matcher = ignore::IgnoreMatcher::from_root(&root).expect("build matcher");
        assert!(matcher.is_ignored(&root.join("ignored"), true));
        assert!(matcher.is_ignored(&root.join("file.tmp"), false));
        assert!(!matcher.is_ignored(&root.join("keep.txt"), false));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "ignore")]
    #[test]
    fn nested_gitignore_rules_are_loaded_on_demand() {
        let root = temp_root("nested-ignore");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("nested")).expect("create nested directory");
        fs::write(root.join("nested/.gitignore"), "*.tmp\n").expect("write nested ignore");
        let matcher = ignore::IgnoreMatcher::from_root(&root).expect("build matcher");
        assert!(matcher.is_ignored(&root.join("nested/file.tmp"), false));
        assert!(!matcher.is_ignored(&root.join("nested/file.txt"), false));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "ignore")]
    #[test]
    fn shared_ignore_matcher_loads_fdignore_rules() {
        let root = temp_root("fdignore");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join(".fdignore"), "build/\n").expect("write fdignore");
        let matcher = ignore::IgnoreMatcher::from_root(&root).expect("build matcher");
        assert!(matcher.is_ignored(&root.join("build"), true));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "git")]
    #[test]
    fn parses_git_status_relative_to_the_repository_root() {
        let root = Path::new("/tmp/fsx-test-repo");
        let statuses = git::parse_porcelain_v1_z_at(b"M  src/main.rs\0!! target/\0", root);
        assert_eq!(
            statuses.get(&root.join("src/main.rs")),
            Some(&"M ".to_string())
        );
        assert_eq!(statuses.get(&root.join("target/")), Some(&"!!".to_string()));
        assert_eq!(
            git::parse_status_pair("??"),
            git::StatusPair {
                staged: ' ',
                worktree: '?'
            }
        );
        assert_eq!(git::display_status_symbol(' '), '-');
        assert_eq!(git::display_status_symbol('!'), 'I');
    }

    #[cfg(feature = "colors")]
    #[test]
    fn shared_colors_prefer_suffixes_over_executable_fallback() {
        let colors = colors::parse_ls_colors_value("ex=1;32:*.rs=38;5;208");
        assert_eq!(
            colors::color_code_for_path("main.rs", false, false, false, true, &colors),
            Some("38;5;208")
        );
        assert_eq!(
            colors::color_code_for_path("tool", false, false, false, true, &colors),
            Some("1;32")
        );
    }

    #[cfg(all(feature = "git", unix))]
    #[test]
    fn git_status_parser_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let statuses = git::parse_porcelain_v1_z_at(b"M  bad\xff\0", Path::new("/tmp"));
        assert!(statuses.keys().any(|path| {
            path.file_name()
                .is_some_and(|name| name.as_bytes() == b"bad\xff")
        }));
    }
}
