use rusqlite::Connection;

#[test]
fn stats_delta_migration_is_idempotent_and_skips_noops() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE entries(dir_id INTEGER, kind INTEGER, allocated_size INTEGER, link_count INTEGER, mtime INTEGER);
         CREATE TABLE dir_stats(dir_id INTEGER PRIMARY KEY, allocated_size INTEGER, files INTEGER, dirs INTEGER, missing_sizes INTEGER, missing_hardlink_metadata INTEGER);
         CREATE TABLE writes(n INTEGER);
         CREATE TRIGGER count_updates AFTER UPDATE ON dir_stats BEGIN INSERT INTO writes VALUES(1); END;
         CREATE TRIGGER entries_stats_au AFTER UPDATE ON entries BEGIN INSERT INTO writes VALUES(100); END;
         INSERT INTO entries VALUES(1, 0, 4096, 1, 0);
         INSERT INTO dir_stats VALUES(1, 4096, 1, 0, 0, 0);",
    ).unwrap();
    conn.execute_batch(include_str!("stats_update.sql"))
        .unwrap();
    // Older writers use IF NOT EXISTS with this name; retaining it prevents
    // them from adding a second, unconditional stats trigger after an upgrade.
    conn.execute_batch("CREATE TRIGGER IF NOT EXISTS entries_stats_au AFTER UPDATE ON entries BEGIN INSERT INTO writes VALUES(100); END;").unwrap();
    assert!(conn.query_row("SELECT instr(sql, 'fsx-net-delta-v2') > 0 FROM sqlite_master WHERE name='entries_stats_au'", [], |row| row.get::<_, bool>(0)).unwrap());
    conn.execute_batch(include_str!("stats_update.sql"))
        .unwrap();
    conn.execute(
        "UPDATE entries SET mtime=1, allocated_size=4096, link_count=2",
        [],
    )
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM writes", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    conn.execute("UPDATE entries SET allocated_size=8192", [])
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM writes", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT allocated_size FROM dir_stats", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        8192
    );
    conn.execute(
        "UPDATE entries SET kind=1, allocated_size=NULL, link_count=NULL",
        [],
    )
    .unwrap();
    let totals = conn.query_row("SELECT allocated_size, files, dirs, missing_sizes, missing_hardlink_metadata FROM dir_stats WHERE dir_id=1", [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))).unwrap();
    assert_eq!(totals, (0, 0, 1, 1, 1));
    conn.execute(
        "UPDATE entries SET dir_id=2, kind=0, allocated_size=1024, link_count=1",
        [],
    )
    .unwrap();
    let old = conn.query_row("SELECT files+dirs+missing_sizes+missing_hardlink_metadata+allocated_size FROM dir_stats WHERE dir_id=1", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(old, 0);
    assert_eq!(
        conn.query_row(
            "SELECT allocated_size FROM dir_stats WHERE dir_id=2",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1024
    );
}
