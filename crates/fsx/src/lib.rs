//! Shared filesystem primitives used by the fsx command-line tools.
//!
//! The crate reports filesystem facts. Consumers remain responsible for their
//! own CLI policy, sorting, presentation layout, search behavior, and transfer
//! decisions.

pub mod build_info;
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
pub use error::{Error, ErrorCode, Result, code_for_message};
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

    #[cfg(feature = "scan")]
    type AggregateSignature = (u64, u64, u64, u64, bool);

    #[cfg(feature = "scan")]
    type TopLevelSignature = (
        AggregateSignature,
        Vec<(std::ffi::OsString, AggregateSignature)>,
        bool,
        u64,
        bool,
    );

    #[cfg(feature = "scan")]
    fn aggregate_signature(aggregate: &scan::Aggregate) -> AggregateSignature {
        (
            aggregate.logical_size,
            aggregate.allocated_size,
            aggregate.files,
            aggregate.dirs,
            aggregate.overflowed,
        )
    }

    #[cfg(feature = "scan")]
    fn top_level_signature(snapshot: &scan::TopLevelScanSnapshot) -> TopLevelSignature {
        let mut children = snapshot
            .children
            .iter()
            .map(|(name, aggregate)| (name.clone(), aggregate_signature(aggregate)))
            .collect::<Vec<_>>();
        children.sort_by(|left, right| left.0.cmp(&right.0));
        (
            aggregate_signature(&snapshot.root),
            children,
            snapshot.complete,
            snapshot.errors,
            snapshot.overflowed,
        )
    }

    #[cfg(all(feature = "scan", unix))]
    fn relative_path_between(from: &Path, to: &Path) -> PathBuf {
        let from_components = from.components().collect::<Vec<_>>();
        let to_components = to.components().collect::<Vec<_>>();
        let common = from_components
            .iter()
            .zip(&to_components)
            .take_while(|(left, right)| left == right)
            .count();
        let mut relative = PathBuf::new();
        for _ in &from_components[common..] {
            relative.push("..");
        }
        for component in &to_components[common..] {
            relative.push(component.as_os_str());
        }
        relative
    }

    #[cfg(all(feature = "scan", unix))]
    struct PermissionRestore {
        path: PathBuf,
        permissions: Option<std::fs::Permissions>,
    }

    #[cfg(all(feature = "scan", unix))]
    impl Drop for PermissionRestore {
        fn drop(&mut self) {
            if let Some(permissions) = self.permissions.take() {
                let _ = fs::set_permissions(&self.path, permissions);
            }
        }
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

    #[cfg(feature = "scan")]
    #[test]
    fn top_level_scan_retains_only_immediate_aggregates() {
        let root = temp_root("top-level-scan");
        let _ = fs::remove_dir_all(&root);
        let mut nested = root.join("child");
        fs::create_dir_all(&nested).expect("create top-level directory");
        for depth in 0..64 {
            nested.push(format!("nested-{depth}"));
            fs::create_dir(&nested).expect("create nested directory");
        }
        fs::write(nested.join("file"), b"payload").expect("create nested file");

        let snapshot = scan::scan_top_level(&scan::ScanRequest {
            root: root.clone(),
            count_files: true,
            count_dirs: true,
            threads: 4,
            ..scan::ScanRequest::default()
        });

        assert!(snapshot.complete);
        assert_eq!(snapshot.children.len(), 1);
        assert_eq!(snapshot.root.dirs, 65);
        assert_eq!(snapshot.root.files, 1);
        let child = snapshot
            .children
            .get(std::ffi::OsStr::new("child"))
            .unwrap();
        assert_eq!(child.dirs, 64);
        assert_eq!(child.files, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(all(feature = "scan", unix))]
    #[test]
    fn top_level_scan_resolves_relative_roots_without_losing_buckets() {
        let root = temp_root("top-level-relative-root");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("child/deep")).expect("create relative-root tree");
        fs::write(root.join("child/deep/file"), b"relative").expect("create relative-root file");
        let cwd = fs::canonicalize(std::env::current_dir().expect("read current directory"))
            .expect("canonicalize current directory");
        let canonical_root = fs::canonicalize(&root).expect("canonicalize fixture root");
        let relative_root = relative_path_between(&cwd, &canonical_root);
        assert!(!relative_root.is_absolute());

        let snapshot = scan::scan_top_level(&scan::ScanRequest {
            root: relative_root,
            size_mode: scan::SizeMode::Logical,
            count_files: true,
            count_dirs: true,
            threads: 4,
            ..scan::ScanRequest::default()
        });

        assert!(snapshot.complete);
        assert_eq!(snapshot.errors, 0);
        assert_eq!(snapshot.root.logical_size, 8);
        assert_eq!(snapshot.root.dirs, 2);
        assert_eq!(snapshot.root.files, 1);
        assert_eq!(snapshot.children.len(), 1);
        let child = &snapshot.children[std::ffi::OsStr::new("child")];
        assert_eq!(child.logical_size, 8);
        assert_eq!(child.dirs, 1);
        assert_eq!(child.files, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "scan")]
    #[test]
    fn top_level_scan_hidden_policy_applies_to_entries_and_subtrees() {
        let root = temp_root("top-level-hidden");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("child")).expect("create visible directory");
        fs::create_dir_all(root.join(".hidden-dir")).expect("create hidden directory");
        fs::write(root.join("visible-root"), b"v").expect("create visible root file");
        fs::write(root.join(".hidden-file"), b"hhh").expect("create hidden root file");
        fs::write(root.join("child/visible-child"), b"vv").expect("create visible child file");
        fs::write(root.join("child/.hidden-child"), b"hhhh").expect("create hidden child file");
        fs::write(root.join(".hidden-dir/nested"), b"hhhhh")
            .expect("create file in hidden subtree");

        let request = scan::ScanRequest {
            root: root.clone(),
            size_mode: scan::SizeMode::Logical,
            count_files: true,
            count_dirs: true,
            show_hidden: false,
            threads: 4,
            ..scan::ScanRequest::default()
        };
        let visible = scan::scan_top_level(&request);
        assert!(visible.complete);
        assert_eq!(visible.root.logical_size, 3);
        assert_eq!(visible.root.dirs, 1);
        assert_eq!(visible.root.files, 2);
        assert!(
            !visible
                .children
                .contains_key(std::ffi::OsStr::new(".hidden-file"))
        );
        assert!(
            !visible
                .children
                .contains_key(std::ffi::OsStr::new(".hidden-dir"))
        );
        let visible_child = &visible.children[std::ffi::OsStr::new("child")];
        assert_eq!(visible_child.logical_size, 2);
        assert_eq!(visible_child.files, 1);

        let with_hidden = scan::scan_top_level(&scan::ScanRequest {
            show_hidden: true,
            ..request
        });
        assert!(with_hidden.complete);
        assert_eq!(with_hidden.root.logical_size, 15);
        assert_eq!(with_hidden.root.dirs, 2);
        assert_eq!(with_hidden.root.files, 5);
        assert!(
            with_hidden
                .children
                .contains_key(std::ffi::OsStr::new(".hidden-file"))
        );
        let hidden_directory = &with_hidden.children[std::ffi::OsStr::new(".hidden-dir")];
        assert_eq!(hidden_directory.logical_size, 5);
        assert_eq!(hidden_directory.files, 1);
        let visible_child = &with_hidden.children[std::ffi::OsStr::new("child")];
        assert_eq!(visible_child.logical_size, 6);
        assert_eq!(visible_child.files, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(all(feature = "scan", unix))]
    #[test]
    fn top_level_scan_root_symlink_obeys_follow_policy() {
        let root = temp_root("top-level-root-symlink");
        let _ = fs::remove_dir_all(&root);
        let target = root.join("target");
        let link = root.join("root-link");
        fs::create_dir_all(target.join("child")).expect("create symlink target tree");
        fs::write(target.join("file"), vec![b'f'; 8192]).expect("create target file");
        std::os::unix::fs::symlink(&target, &link).expect("create root symlink");

        let request = scan::ScanRequest {
            root: link.clone(),
            size_mode: scan::SizeMode::Allocated,
            count_files: true,
            count_dirs: true,
            symlinks: scan::SymlinkMode::DoNotFollow,
            threads: 4,
            ..scan::ScanRequest::default()
        };
        let not_followed = scan::scan_top_level(&request);
        let link_size = metadata::allocated_size(&fs::symlink_metadata(&link).unwrap());
        assert!(not_followed.complete);
        assert_eq!(not_followed.root.allocated_size, link_size);
        assert_eq!(not_followed.root.dirs, 0);
        assert_eq!(not_followed.root.files, 0);
        assert!(not_followed.children.is_empty());

        let followed = scan::scan_top_level(&scan::ScanRequest {
            symlinks: scan::SymlinkMode::Follow,
            ..request
        });
        let target_size = metadata::allocated_size(&fs::metadata(&link).unwrap());
        let child_size = metadata::allocated_size(&fs::metadata(target.join("child")).unwrap());
        let file_size = metadata::allocated_size(&fs::metadata(target.join("file")).unwrap());
        assert!(followed.complete);
        assert_eq!(followed.errors, 0);
        assert_eq!(
            followed.root.allocated_size,
            target_size + child_size + file_size
        );
        assert_eq!(followed.root.dirs, 1);
        assert_eq!(followed.root.files, 1);
        assert_eq!(followed.children.len(), 2);
        assert_eq!(
            followed.children[std::ffi::OsStr::new("child")].allocated_size,
            child_size
        );
        assert_eq!(
            followed.children[std::ffi::OsStr::new("file")].allocated_size,
            file_size
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(all(feature = "scan", unix))]
    #[test]
    fn top_level_scan_reports_unreadable_subtree_and_preserves_partial_totals() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("top-level-unreadable");
        let _ = fs::remove_dir_all(&root);
        let good = root.join("good");
        let blocked = root.join("blocked");
        fs::create_dir_all(&good).expect("create readable directory");
        fs::create_dir_all(&blocked).expect("create directory to restrict");
        fs::write(good.join("file"), b"readable").expect("create readable file");
        fs::write(blocked.join("file"), b"unreadable").expect("create restricted file");

        let original_permissions = fs::symlink_metadata(&blocked)
            .expect("read original permissions")
            .permissions();
        fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000))
            .expect("restrict directory");
        let restore = PermissionRestore {
            path: blocked.clone(),
            permissions: Some(original_permissions),
        };
        if fs::read_dir(&blocked).is_ok() {
            drop(restore);
            let _ = fs::remove_dir_all(root);
            return;
        }

        let snapshot = scan::scan_top_level(&scan::ScanRequest {
            root: root.clone(),
            size_mode: scan::SizeMode::Allocated,
            count_files: true,
            count_dirs: true,
            threads: 4,
            ..scan::ScanRequest::default()
        });
        let root_size = metadata::allocated_size(&fs::symlink_metadata(&root).unwrap());
        let good_size = metadata::allocated_size(&fs::symlink_metadata(&good).unwrap());
        let blocked_size = metadata::allocated_size(&fs::symlink_metadata(&blocked).unwrap());
        let file_size = metadata::allocated_size(&fs::symlink_metadata(good.join("file")).unwrap());

        assert!(!snapshot.complete);
        assert_eq!(snapshot.errors, 1);
        assert_eq!(snapshot.root.dirs, 2);
        assert_eq!(snapshot.root.files, 1);
        assert_eq!(
            snapshot.root.allocated_size,
            root_size + good_size + blocked_size + file_size
        );
        let good_stats = &snapshot.children[std::ffi::OsStr::new("good")];
        assert_eq!(good_stats.files, 1);
        assert_eq!(good_stats.allocated_size, good_size + file_size);
        let blocked_stats = &snapshot.children[std::ffi::OsStr::new("blocked")];
        assert_eq!(blocked_stats.files, 0);
        assert_eq!(blocked_stats.dirs, 0);
        assert_eq!(blocked_stats.allocated_size, blocked_size);

        drop(restore);
        assert!(fs::read_dir(&blocked).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(all(feature = "scan", unix))]
    #[test]
    fn top_level_scan_is_exact_across_thread_counts() {
        let root = temp_root("top-level-thread-parity");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/deep")).expect("create first tree");
        fs::create_dir_all(root.join("b")).expect("create second tree");
        fs::create_dir_all(root.join(".hidden")).expect("create hidden tree");
        fs::write(root.join("a/shared"), vec![b's'; 8192]).expect("create shared file");
        fs::hard_link(root.join("a/shared"), root.join("a/shared-again"))
            .expect("create local hardlink");
        fs::hard_link(root.join("a/shared"), root.join("b/shared"))
            .expect("create cross-child hardlink");
        fs::write(root.join("a/deep/unique"), vec![b'u'; 16384]).expect("create unique file");
        fs::write(root.join(".hidden/file"), b"hidden").expect("create hidden file");
        std::os::unix::fs::symlink("missing", root.join("dangling"))
            .expect("create dangling symlink");

        let base_request = scan::ScanRequest {
            root: root.clone(),
            size_mode: scan::SizeMode::Allocated,
            count_files: true,
            count_dirs: true,
            hardlinks: scan::HardlinkMode::DeduplicateCandidates,
            show_hidden: true,
            ..scan::ScanRequest::default()
        };
        let available = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1);
        let mut expected = None;
        for threads in [1, 4, available] {
            let snapshot = scan::scan_top_level(&scan::ScanRequest {
                threads,
                ..base_request.clone()
            });
            assert!(snapshot.complete, "threads={threads}");
            let signature = top_level_signature(&snapshot);
            if let Some(expected) = &expected {
                assert_eq!(&signature, expected, "threads={threads}");
            } else {
                expected = Some(signature);
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(all(feature = "scan", unix))]
    #[test]
    fn top_level_scan_deduplicates_hardlinks_per_scope() {
        let root = temp_root("top-level-hardlinks");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a")).expect("create first directory");
        fs::create_dir_all(root.join("b")).expect("create second directory");
        fs::write(root.join("a/first"), vec![b'x'; 8192]).expect("create source file");
        fs::hard_link(root.join("a/first"), root.join("a/second"))
            .expect("create same-child hardlink");
        fs::hard_link(root.join("a/first"), root.join("b/third"))
            .expect("create cross-child hardlink");
        fs::write(root.join("a/unique"), vec![b'u'; 16384]).expect("create unique file");

        let request = scan::ScanRequest {
            root: root.clone(),
            size_mode: scan::SizeMode::Allocated,
            count_files: true,
            count_dirs: true,
            hardlinks: scan::HardlinkMode::DeduplicateCandidates,
            threads: 4,
            ..scan::ScanRequest::default()
        };
        let snapshot = scan::scan_top_level(&request);
        let root_size = metadata::allocated_size(&fs::symlink_metadata(&root).unwrap());
        let a_size = metadata::allocated_size(&fs::symlink_metadata(root.join("a")).unwrap());
        let b_size = metadata::allocated_size(&fs::symlink_metadata(root.join("b")).unwrap());
        let file_size =
            metadata::allocated_size(&fs::symlink_metadata(root.join("a/first")).unwrap());
        let unique_size =
            metadata::allocated_size(&fs::symlink_metadata(root.join("a/unique")).unwrap());

        assert!(snapshot.complete);
        assert_eq!(snapshot.errors, 0);
        assert!(!snapshot.overflowed);
        assert_eq!(snapshot.children.len(), 2);
        assert!(file_size > 0);
        assert!(unique_size > 0);
        assert_eq!(
            snapshot.root.allocated_size,
            root_size + a_size + b_size + file_size + unique_size
        );
        assert_eq!(snapshot.root.logical_size, 0);
        assert_eq!(snapshot.root.dirs, 2);
        assert_eq!(snapshot.root.files, 4);
        assert_eq!(
            snapshot.children[std::ffi::OsStr::new("a")].allocated_size,
            a_size + file_size + unique_size
        );
        assert_eq!(snapshot.children[std::ffi::OsStr::new("a")].dirs, 0);
        assert_eq!(snapshot.children[std::ffi::OsStr::new("a")].files, 3);
        assert_eq!(
            snapshot.children[std::ffi::OsStr::new("b")].allocated_size,
            b_size + file_size
        );
        assert_eq!(snapshot.children[std::ffi::OsStr::new("b")].dirs, 0);
        assert_eq!(snapshot.children[std::ffi::OsStr::new("b")].files, 1);

        let counted = scan::scan_top_level(&scan::ScanRequest {
            hardlinks: scan::HardlinkMode::CountEveryEntry,
            ..request
        });
        assert_eq!(
            counted.root.allocated_size,
            root_size + a_size + b_size + file_size * 3 + unique_size
        );
        assert_eq!(counted.root.dirs, 2);
        assert_eq!(counted.root.files, 4);
        assert_eq!(
            counted.children[std::ffi::OsStr::new("a")].allocated_size,
            a_size + file_size * 2 + unique_size
        );
        assert_eq!(counted.children[std::ffi::OsStr::new("a")].files, 3);
        assert_eq!(
            counted.children[std::ffi::OsStr::new("b")].allocated_size,
            b_size + file_size
        );
        assert_eq!(counted.children[std::ffi::OsStr::new("b")].files, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "scan")]
    #[test]
    fn top_level_scan_reports_a_missing_root_as_incomplete() {
        let root = temp_root("missing-top-level-scan");
        let _ = fs::remove_dir_all(&root);
        let snapshot = scan::scan_top_level(&scan::ScanRequest {
            root,
            count_files: true,
            count_dirs: true,
            ..scan::ScanRequest::default()
        });
        assert!(!snapshot.complete);
        assert!(snapshot.errors >= 1);
        assert!(snapshot.children.is_empty());
    }

    #[cfg(feature = "scan")]
    #[test]
    fn scan_count_flags_are_independent() {
        let root = temp_root("scan-count-flags");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("child")).expect("create directory");
        fs::write(root.join("child/file"), b"file").expect("create file");

        let files_only = scan::scan(&scan::ScanRequest {
            root: root.clone(),
            count_files: true,
            ..scan::ScanRequest::default()
        });
        assert_eq!(files_only.aggregates[&root].files, 1);
        assert_eq!(files_only.aggregates[&root].dirs, 0);

        let dirs_only = scan::scan(&scan::ScanRequest {
            root: root.clone(),
            count_dirs: true,
            ..scan::ScanRequest::default()
        });
        assert_eq!(dirs_only.aggregates[&root].files, 0);
        assert_eq!(dirs_only.aggregates[&root].dirs, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "scan")]
    #[test]
    fn scan_populates_only_the_requested_size_metric() {
        let root = temp_root("scan-size-mode");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("child")).expect("create directory");
        fs::write(root.join("child/file"), b"logical bytes").expect("create file");

        let logical = scan::scan(&scan::ScanRequest {
            root: root.clone(),
            size_mode: scan::SizeMode::Logical,
            ..scan::ScanRequest::default()
        });
        assert_eq!(logical.aggregates[&root].logical_size, 13);
        assert_eq!(logical.aggregates[&root].allocated_size, 0);

        let allocated = scan::scan(&scan::ScanRequest {
            root: root.clone(),
            size_mode: scan::SizeMode::Allocated,
            ..scan::ScanRequest::default()
        });
        assert_eq!(allocated.aggregates[&root].logical_size, 0);
        assert!(allocated.aggregates[&root].allocated_size > 0);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(all(feature = "scan", unix))]
    #[test]
    fn followed_directory_cycles_and_aliases_have_one_stable_representative() {
        let root = temp_root("top-level-follow-cycle");
        let _ = fs::remove_dir_all(&root);
        let real = root.join("a-real");
        fs::create_dir_all(&real).expect("create target directory");
        fs::write(real.join("file"), b"payload").expect("create target file");
        std::os::unix::fs::symlink("..", real.join("back")).expect("create relative cycle");
        std::os::unix::fs::symlink("a-real", root.join("z-alias"))
            .expect("create duplicate directory alias");

        let snapshot = scan::scan_top_level(&scan::ScanRequest {
            root: root.clone(),
            size_mode: scan::SizeMode::Logical,
            count_files: true,
            count_dirs: true,
            symlinks: scan::SymlinkMode::Follow,
            max_depth: Some(100),
            threads: 8,
            ..scan::ScanRequest::default()
        });

        assert!(snapshot.complete);
        assert_eq!(snapshot.errors, 0);
        assert_eq!(snapshot.root.logical_size, 7);
        assert_eq!(snapshot.root.dirs, 1);
        assert_eq!(snapshot.root.files, 1);
        assert_eq!(snapshot.children.len(), 1);
        assert!(
            snapshot
                .children
                .contains_key(std::ffi::OsStr::new("a-real"))
        );
        assert!(
            !snapshot
                .children
                .contains_key(std::ffi::OsStr::new("z-alias"))
        );
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
