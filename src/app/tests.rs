use super::*;
use rusqlite::Connection;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

fn base_opts() -> Options {
    Options {
        timeout_dur: Duration::from_secs(6),
        timeout_explicit: false,
        force_pattern_mode: false,
        long_format: false,
        long_extended: false,
        sizes: false,
        counts: false,
        regex_mode: false,
        sort_field: None,
        sort_order: None,
        limit: None,
        reverse: false,
        no_recurse: false,
        follow_links: false,
        respect_ignore: false,
        visible_only: true,
        threads_override: 8,
        threads_explicit: false,
        cache_output: false,
        snapshot_cache: false,
        snapshot_refresh: false,
        index_mode: false,
        index_if_watched: false,
        index_binary: false,
        recent_limit: None,
        index_refresh: None,
        index_snapshot: None,
        index_purge: None,
        watch: false,
        watch_status: false,
        watch_metrics: None,
        absolute_paths: false,
        force_dir: false,
        force_file: false,
        force_full: false,
        classify: false,
        color_when: ColorWhen::Auto,
        hyperlinks: false,
        highlight_match: false,
        contains_all: false,
        path_override: None,
        positional: Vec::new(),
    }
}

#[test]
fn media_root_prefers_single_thread() {
    assert!(root_prefers_single_thread(Path::new("/media")));
    assert!(root_prefers_single_thread(Path::new("/media/disk")));
    assert!(!root_prefers_single_thread(Path::new("/mnt")));
    assert!(!root_prefers_single_thread(Path::new("/home/lewis")));
}

#[test]
fn explicit_threads_override_media_default() {
    let mut opts = base_opts();
    opts.threads_override = 32;
    opts.threads_explicit = true;
    let spec = ContainsAllSpec {
        terms: vec!["x".to_string()],
        root: PathBuf::from("/media/disk"),
    };
    assert_eq!(effective_threads_override(&opts, Some(&spec)), 32);
    assert_eq!(recent_refresh_threads(&opts, Path::new("/media/disk")), 32);
}

#[test]
fn recent_refresh_keeps_media_serial_by_default() {
    let opts = base_opts();
    assert_eq!(recent_refresh_threads(&opts, Path::new("/media/disk")), 1);
    assert!((1..=16).contains(&recent_refresh_threads(&opts, Path::new("/home/lewis"))));
}

#[test]
fn manifest_optional_i64_round_trip() {
    for value in [None, Some(0), Some(42), Some(i64::MAX)] {
        assert_eq!(decode_optional_i64(encode_optional_i64(value)), value);
    }
}

#[test]
fn sql_prefilter_skips_unrestricted_wildcards() {
    assert!(sql_prefilter_for_term("*", false, true, "path", true).is_none());
    assert!(sql_prefilter_for_term("**", false, true, "path", true).is_none());
    assert!(sql_prefilter_for_term("passwords", false, true, "path", true).is_some());
    assert!(sql_prefilter_for_term("pass*", false, true, "path", true).is_some());
}

#[test]
fn scanned_index_entries_are_deduplicated_before_refresh() {
    let entry = |path: &str, kind| ScannedIndexEntry {
        path: path.to_string(),
        kind,
        mtime: Some(1),
        size: Some(2),
        activity: Some(3),
    };
    let mut entries = vec![
        entry("/root/repeated", 0),
        entry("/root/other", 1),
        entry("/root/repeated", 0),
    ];

    sort_and_dedup_scanned_index_entries(&mut entries);

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, "/root/other");
    assert_eq!(entries[1].path, "/root/repeated");
}

#[test]
fn indexed_scan_honors_startup_cancellation() {
    static CANCEL: AtomicBool = AtomicBool::new(true);
    let root = std::env::temp_dir().join(format!(
        "unearth-cancelled-scan-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/file"), b"test").unwrap();

    let result = scan_index_root_cancellable(&root, &normalize_index_dir(&root), 1, Some(&CANCEL));

    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(result.unwrap_err(), "index scan interrupted");
}

#[test]
fn index_batch_insert_is_idempotent_for_live_entries() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE entries (
                 id INTEGER PRIMARY KEY,
                 dir_id INTEGER NOT NULL,
                 name_id INTEGER NOT NULL,
                 kind INTEGER NOT NULL,
                 mtime INTEGER,
                 size INTEGER,
                 activity INTEGER,
                 event_kind INTEGER,
                 actor_id INTEGER,
                 UNIQUE(dir_id, name_id, kind)
             );
             INSERT INTO entries(
                 dir_id, name_id, kind, mtime, size, activity, event_kind, actor_id
             ) VALUES (10, 20, 0, 1, 2, 3, 4, 99);",
    )
    .unwrap();
    let tx = conn.transaction().unwrap();

    insert_index_entries(
        &tx,
        &[PendingIndexEntry {
            dir_id: 10,
            name_id: 20,
            kind: 0,
            mtime: Some(11),
            size: Some(22),
            activity: Some(33),
        }],
    )
    .unwrap();
    tx.commit().unwrap();

    let row = conn
        .query_row(
            "SELECT mtime, size, activity, event_kind, actor_id FROM entries",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row, (11, 22, 33, 4, 99));
}

#[test]
fn recent_query_separates_terms_from_explicit_path() {
    let mut opts = base_opts();
    opts.positional = vec!["poo".to_string(), "/home/lewis".to_string()];
    let (root, terms) = recent_query_from_opts(&opts).unwrap();
    assert_eq!(root, PathBuf::from("/home/lewis"));
    assert_eq!(terms, vec!["poo"]);

    opts.positional = vec!["poo".to_string()];
    let (root, terms) = recent_query_from_opts(&opts).unwrap();
    assert_eq!(root, PathBuf::from("."));
    assert_eq!(terms, vec!["poo"]);

    opts.positional = vec!["poo".to_string(), "~".to_string()];
    let (root, terms) = recent_query_from_opts(&opts).unwrap();
    assert_eq!(root, PathBuf::from(expand_home_path("~")));
    assert_eq!(terms, vec!["poo"]);

    opts.positional = vec!["poo".to_string()];
    opts.path_override = Some("bare_folder".to_string());
    let (root, terms) = recent_query_from_opts(&opts).unwrap();
    assert_eq!(root, PathBuf::from("bare_folder"));
    assert_eq!(terms, vec!["poo"]);
}

#[test]
fn root_index_excludes_volatile_system_trees() {
    assert!(is_root_index_excluded_path("/", "/dev"));
    assert!(is_root_index_excluded_path("/", "/dev/null"));
    assert!(is_root_index_excluded_path("/", "/proc/1/status"));
    assert!(is_root_index_excluded_path("/", "/sys/class"));
    assert!(is_root_index_excluded_path("/", "/run/user"));
    assert!(!is_root_index_excluded_path("/", "/media"));
    assert!(!is_root_index_excluded_path("/", "/home/lewis"));
    assert!(!is_root_index_excluded_path("/dev", "/dev/null"));
}

#[test]
fn root_index_excludes_unearths_own_cache() {
    let cache = unearth_cache_dir().expect("test environment has a cache directory");
    let cache_key = normalize_index_dir(&cache);
    assert!(is_root_index_excluded_path("/", &cache_key));
    assert!(is_root_index_excluded_path(
        "/home",
        &format!("{cache_key}/index/unearth.db-wal")
    ));
    assert!(is_root_index_prune_child("/", &cache));
    assert!(!is_root_index_excluded_path(
        "/",
        &format!("{cache_key}-backup")
    ));
}

#[test]
fn parent_file_uri_selects_files_and_directories() {
    assert_eq!(
        parent_file_uri(
            "/home/lewis/Videos/obs/",
            "/home/lewis/Videos/obs/2026-08-01 13-33-34.mp4",
        ),
        "file:///home/lewis/Videos/obs/?select=/home/lewis/Videos/obs/2026-08-01%2013-33-34.mp4"
    );
    assert_eq!(
        parent_file_uri("/home/lewis/Videos/", "/home/lewis/Videos/obs/"),
        "file:///home/lewis/Videos/?select=/home/lewis/Videos/obs/"
    );
}

#[test]
fn index_diff_finds_additions_removals_and_kind_changes() {
    let scanned = vec![
        ScannedIndexEntry {
            path: "/root/added".to_string(),
            kind: 0,
            mtime: None,
            size: None,
            activity: None,
        },
        ScannedIndexEntry {
            path: "/root/changed".to_string(),
            kind: 1,
            mtime: None,
            size: None,
            activity: None,
        },
        ScannedIndexEntry {
            path: "/root/kept".to_string(),
            kind: 0,
            mtime: None,
            size: None,
            activity: None,
        },
    ];
    let existing = vec![
        ExistingIndexEntry {
            dir_id: 10,
            name_id: 20,
            path: Cow::Borrowed("/root/changed"),
            kind: 0,
            mtime: None,
            size: None,
            activity: None,
        },
        ExistingIndexEntry {
            dir_id: 11,
            name_id: 21,
            path: Cow::Borrowed("/root/kept"),
            kind: 0,
            mtime: None,
            size: None,
            activity: None,
        },
        ExistingIndexEntry {
            dir_id: 12,
            name_id: 22,
            path: Cow::Borrowed("/root/removed"),
            kind: 0,
            mtime: None,
            size: None,
            activity: None,
        },
    ];

    let (removed, added, updated) = diff_index_entries(&scanned, &existing);

    assert_eq!(removed, vec![0, 2]);
    assert_eq!(added, vec![0, 1]);
    assert!(updated.is_empty());
}

#[test]
fn index_diff_detects_same_count_rename() {
    let scanned = vec![ScannedIndexEntry {
        path: "/root/new-name".to_string(),
        kind: 0,
        mtime: None,
        size: None,
        activity: None,
    }];
    let existing = vec![ExistingIndexEntry {
        dir_id: 42,
        name_id: 52,
        path: Cow::Borrowed("/root/old-name"),
        kind: 0,
        mtime: None,
        size: None,
        activity: None,
    }];

    let (removed, added, updated) = diff_index_entries(&scanned, &existing);

    assert_eq!(removed, vec![0]);
    assert_eq!(added, vec![0]);
    assert!(updated.is_empty());
}
