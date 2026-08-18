use super::*;
use rusqlite::Connection;
use std::borrow::Cow;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
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
        case_sensitive: false,
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
        live_only: false,
        index_mode: false,
        index_if_watched: false,
        index_binary: false,
        recent_limit: None,
        index_refresh: None,
        index_snapshot: None,
        index_purge: None,
        watch: false,
        watch_status: false,
        watch_status_json: false,
        watch_metrics: None,
        watch_metrics_os: None,
        watch_metrics_ttl: 300,
        watch_metrics_max_bytes: 16 * 1024 * 1024,
        absolute_paths: false,
        lossless_paths: false,
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
        path_override_os: None,
        positional_os: Vec::new(),
        index_refresh_os: None,
        index_snapshot_os: None,
        index_purge_os: None,
    }
}

#[test]
fn incompatible_daemon_query_response_requests_local_fallback() {
    let (client, mut server) = UnixStream::pair().unwrap();
    server.write_all(b"UNRS0001").unwrap();
    drop(server);

    assert!(begin_query_results(client).unwrap().is_none());
}

#[test]
fn current_daemon_query_response_starts_record_reader() {
    let (client, mut server) = UnixStream::pair().unwrap();
    server.write_all(QUERY_RESPONSE_MAGIC).unwrap();
    server.write_all(&[0]).unwrap();
    drop(server);

    assert!(begin_query_results(client).unwrap().is_some());
}

#[test]
fn ls_colors_suffix_rules_preserve_first_match_precedence() {
    let colors = parse_ls_colors_value(
        "*boggle*=complex:*.rs=suffix:*.tar.gz=archive:di=dir:ln=link:ex=exec",
    );
    let result = |path: &str| SearchResult {
        path: path.to_string(),
        path_encoded: false,
        is_dir: false,
        is_symlink: false,
        metadata: None,
        indexed_activity_nanos: None,
        indexed_size: None,
    };

    assert_eq!(
        color_code_for_path(&result("boggle.rs"), &colors),
        Some("complex")
    );
    assert_eq!(
        color_code_for_path(&result("notes.rs"), &colors),
        Some("suffix")
    );
    assert_eq!(
        color_code_for_path(&result("archive.tar.gz"), &colors),
        Some("archive")
    );
    assert_eq!(color_code_for_path(&result("plain"), &colors), None);
}

#[test]
fn indexed_executables_use_live_mode_for_color_and_classification() {
    let colors = parse_ls_colors_value("fi=file:ex=exec");
    let path = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let result = SearchResult {
        path,
        path_encoded: false,
        is_dir: false,
        is_symlink: false,
        metadata: None,
        indexed_activity_nanos: None,
        indexed_size: None,
    };

    assert_eq!(color_code_for_path(&result, &colors), Some("exec"));
    assert_eq!(decorator_for_res(&result), Some('*'));
}

#[test]
fn media_root_prefers_single_thread() {
    assert!(root_prefers_single_thread(Path::new("/media")));
    assert!(root_prefers_single_thread(Path::new("/media/disk")));
    assert_eq!(
        root_prefers_single_thread(Path::new("/storage")),
        cfg!(target_os = "android")
    );
    assert_eq!(
        root_prefers_single_thread(Path::new("/mnt")),
        cfg!(target_os = "android")
    );
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
    assert!(sql_prefilter_for_term("*", false, false, true, "path", true).is_none());
    assert!(sql_prefilter_for_term("**", false, false, true, "path", true).is_none());
    assert!(sql_prefilter_for_term("passwords", false, false, true, "path", true).is_some());
    assert!(sql_prefilter_for_term("pass*", false, false, true, "path", true).is_some());
    assert!(sql_prefilter_for_term("passwords", false, true, true, "path", true).is_none());
}

#[cfg(unix)]
#[test]
fn lossless_path_transport_round_trips_special_bytes() {
    let original = PathBuf::from(OsString::from_vec(b"/tmp/name-\xff\n%".to_vec()));
    let encoded = fsx::encode_lossless_path(&original);
    let decoded = fsx::decode_lossless_path(&encoded);

    assert_eq!(encoded, "/tmp/name-%FF%0A%25");
    assert_eq!(
        decoded.as_os_str().as_bytes(),
        original.as_os_str().as_bytes()
    );
}

#[test]
fn scanned_index_entries_are_deduplicated_before_refresh() {
    let entry = |path: &str, kind| ScannedIndexEntry {
        path: path.to_string(),
        raw_path: PathBuf::from(path),
        kind,
        mtime: Some(1),
        size: Some(2),
        allocated_size: Some(2),
        activity: Some(3),
        device: None,
        inode: None,
        link_count: Some(1),
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
                 allocated_size INTEGER,
                 activity INTEGER,
                 device INTEGER,
                 inode INTEGER,
                 link_count INTEGER,
                 event_kind INTEGER,
                 actor_id INTEGER,
                 UNIQUE(dir_id, name_id, kind)
             );
             INSERT INTO entries(
                 dir_id, name_id, kind, mtime, size, allocated_size, activity, event_kind, actor_id
             ) VALUES (10, 20, 0, 1, 2, 22, 3, 4, 99);",
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
            allocated_size: Some(22),
            activity: Some(33),
            device: None,
            inode: None,
            link_count: Some(1),
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
fn indexed_directory_stats_aggregate_complete_subtrees() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE dirs (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
         CREATE TABLE entries (
             id INTEGER PRIMARY KEY,
             dir_id INTEGER NOT NULL,
             kind INTEGER NOT NULL,
             size INTEGER
         );
         CREATE INDEX idx_entries_dir ON entries(dir_id);
         INSERT INTO dirs(id, path) VALUES (1, '/root'), (2, '/root/nested');
         INSERT INTO entries(id, dir_id, kind, size) VALUES
             (1, 1, 0, 5),
             (2, 1, 1, 4096),
             (3, 2, 0, 7),
             (4, 2, 2, 11);",
    )
    .unwrap();

    let mut opts = base_opts();
    opts.sizes = true;
    let items = vec![SearchResult {
        path: "/root/".to_string(),
        path_encoded: false,
        is_dir: true,
        is_symlink: false,
        metadata: None,
        indexed_activity_nanos: None,
        indexed_size: None,
    }];
    let mut cache = DirStatsCache::default();

    populate_indexed_dirsize_cache(&conn, "/root", &items, &opts, &mut cache).unwrap();

    assert_eq!(cache.bytes_map.get("/root"), Some(&12));
    assert_eq!(cache.map.get("/root").map(|stats| stats.files), Some(2));
}

#[test]
fn indexed_directory_stats_skip_subtrees_with_missing_sizes() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE dirs (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
         CREATE TABLE entries (
             id INTEGER PRIMARY KEY,
             dir_id INTEGER NOT NULL,
             kind INTEGER NOT NULL,
             size INTEGER
         );
         CREATE INDEX idx_entries_dir ON entries(dir_id);
         INSERT INTO dirs(id, path) VALUES (1, '/root');
         INSERT INTO entries(id, dir_id, kind, size) VALUES (1, 1, 0, NULL);",
    )
    .unwrap();

    let mut opts = base_opts();
    opts.sizes = true;
    let items = vec![SearchResult {
        path: "/root/".to_string(),
        path_encoded: false,
        is_dir: true,
        is_symlink: false,
        metadata: None,
        indexed_activity_nanos: None,
        indexed_size: None,
    }];
    let mut cache = DirStatsCache::default();

    populate_indexed_dirsize_cache(&conn, "/root", &items, &opts, &mut cache).unwrap();

    assert!(!cache.bytes_map.contains_key("/root"));
    assert!(!cache.map.contains_key("/root"));
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
fn root_index_excludes_fsx_own_cache() {
    let cache = unearth_cache_dir().expect("test environment has a cache directory");
    let cache_key = normalize_index_dir(&cache);
    assert!(is_root_index_excluded_path("/", &cache_key));
    assert!(is_root_index_excluded_path(
        "/home",
        &format!("{cache_key}/index/fsx.db-wal")
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
            raw_path: PathBuf::from("/root/added"),
            kind: 0,
            mtime: None,
            size: None,
            allocated_size: None,
            activity: None,
            device: None,
            inode: None,
            link_count: None,
        },
        ScannedIndexEntry {
            path: "/root/changed".to_string(),
            raw_path: PathBuf::from("/root/changed"),
            kind: 1,
            mtime: None,
            size: None,
            allocated_size: None,
            activity: None,
            device: None,
            inode: None,
            link_count: None,
        },
        ScannedIndexEntry {
            path: "/root/kept".to_string(),
            raw_path: PathBuf::from("/root/kept"),
            kind: 0,
            mtime: None,
            size: None,
            allocated_size: None,
            activity: None,
            device: None,
            inode: None,
            link_count: None,
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
            allocated_size: None,
            activity: None,
            device: None,
            inode: None,
            link_count: None,
        },
        ExistingIndexEntry {
            dir_id: 11,
            name_id: 21,
            path: Cow::Borrowed("/root/kept"),
            kind: 0,
            mtime: None,
            size: None,
            allocated_size: None,
            activity: None,
            device: None,
            inode: None,
            link_count: None,
        },
        ExistingIndexEntry {
            dir_id: 12,
            name_id: 22,
            path: Cow::Borrowed("/root/removed"),
            kind: 0,
            mtime: None,
            size: None,
            allocated_size: None,
            activity: None,
            device: None,
            inode: None,
            link_count: None,
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
        raw_path: PathBuf::from("/root/new-name"),
        kind: 0,
        mtime: None,
        size: None,
        allocated_size: None,
        activity: None,
        device: None,
        inode: None,
        link_count: None,
    }];
    let existing = vec![ExistingIndexEntry {
        dir_id: 42,
        name_id: 52,
        path: Cow::Borrowed("/root/old-name"),
        kind: 0,
        mtime: None,
        size: None,
        allocated_size: None,
        activity: None,
        device: None,
        inode: None,
        link_count: None,
    }];

    let (removed, added, updated) = diff_index_entries(&scanned, &existing);

    assert_eq!(removed, vec![0]);
    assert_eq!(added, vec![0]);
    assert!(updated.is_empty());
}
