-- Upgrade the old unconditional subtract/add trigger without rebuilding totals.
BEGIN IMMEDIATE;
DROP TRIGGER IF EXISTS entries_stats_au;
DROP TRIGGER IF EXISTS entries_stats_au_delta_v2;
CREATE TRIGGER entries_stats_au
AFTER UPDATE OF dir_id, kind, allocated_size, link_count ON entries
WHEN new.dir_id = old.dir_id AND (
    new.kind IS NOT old.kind OR new.allocated_size IS NOT old.allocated_size
    OR (new.link_count IS NULL) != (old.link_count IS NULL)
) BEGIN -- fsx-net-delta-v2
    UPDATE dir_stats SET
        allocated_size = allocated_size + COALESCE(new.allocated_size, 0) - COALESCE(old.allocated_size, 0),
        files = files + (new.kind <> 1) - (old.kind <> 1),
        dirs = dirs + (new.kind = 1) - (old.kind = 1),
        missing_sizes = missing_sizes + (new.allocated_size IS NULL) - (old.allocated_size IS NULL),
        missing_hardlink_metadata = missing_hardlink_metadata + (new.link_count IS NULL) - (old.link_count IS NULL)
    WHERE dir_id = new.dir_id;
END;
CREATE TRIGGER IF NOT EXISTS entries_stats_au_move_v2
AFTER UPDATE OF dir_id ON entries WHEN new.dir_id != old.dir_id BEGIN
    UPDATE dir_stats SET
        allocated_size = allocated_size - COALESCE(old.allocated_size, 0),
        files = files - (old.kind <> 1),
        dirs = dirs - (old.kind = 1),
        missing_sizes = missing_sizes - (old.allocated_size IS NULL),
        missing_hardlink_metadata = missing_hardlink_metadata - (old.link_count IS NULL)
    WHERE dir_id = old.dir_id;
    INSERT INTO dir_stats(dir_id, allocated_size, files, dirs, missing_sizes, missing_hardlink_metadata)
    VALUES (new.dir_id, COALESCE(new.allocated_size, 0), new.kind <> 1, new.kind = 1,
            new.allocated_size IS NULL, new.link_count IS NULL)
    ON CONFLICT(dir_id) DO UPDATE SET
        allocated_size = dir_stats.allocated_size + excluded.allocated_size,
        files = dir_stats.files + excluded.files,
        dirs = dir_stats.dirs + excluded.dirs,
        missing_sizes = dir_stats.missing_sizes + excluded.missing_sizes,
        missing_hardlink_metadata = dir_stats.missing_hardlink_metadata + excluded.missing_hardlink_metadata;
END;
COMMIT;
