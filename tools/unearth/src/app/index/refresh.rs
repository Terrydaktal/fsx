use super::*;
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy)]
pub(crate) struct PendingIndexEntry {
    pub(crate) dir_id: i64,
    pub(crate) name_id: i64,
    pub(crate) kind: i64,
    pub(crate) mtime: Option<i64>,
    pub(crate) size: Option<i64>,
    pub(crate) allocated_size: Option<i64>,
    pub(crate) activity: Option<i64>,
    pub(crate) device: Option<i64>,
    pub(crate) inode: Option<i64>,
    pub(crate) link_count: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScannedIndexEntry {
    pub(crate) path: String,
    pub(crate) kind: i64,
    pub(crate) mtime: Option<i64>,
    pub(crate) size: Option<i64>,
    pub(crate) allocated_size: Option<i64>,
    pub(crate) activity: Option<i64>,
    pub(crate) device: Option<i64>,
    pub(crate) inode: Option<i64>,
    pub(crate) link_count: Option<i64>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ExistingIndexEntry<'a> {
    pub(crate) dir_id: i64,
    pub(crate) name_id: i64,
    pub(crate) path: Cow<'a, str>,
    pub(crate) kind: i64,
    pub(crate) mtime: Option<i64>,
    pub(crate) size: Option<i64>,
    pub(crate) allocated_size: Option<i64>,
    pub(crate) activity: Option<i64>,
    pub(crate) device: Option<i64>,
    pub(crate) inode: Option<i64>,
    pub(crate) link_count: Option<i64>,
}

#[derive(Clone, Debug)]
pub(crate) struct IndexDeltaEntry {
    added: bool,
    dir_id: i64,
    name_id: i64,
    pub(crate) kind: i64,
    pub(crate) path: String,
}

fn reject_child_read_error(
    path: &Path,
    error: Option<&dyn std::fmt::Display>,
) -> Result<(), String> {
    match error {
        Some(error) => Err(format!(
            "index scan could not read directory '{}': {error}",
            path.display()
        )),
        None => Ok(()),
    }
}

pub(crate) fn scan_index_root_cancellable(
    root: &Path,
    root_key: &str,
    threads: usize,
    cancel: Option<&'static AtomicBool>,
) -> Result<Vec<ScannedIndexEntry>, String> {
    let mut entries: Vec<ScannedIndexEntry> = WalkDir::new(root)
        .skip_hidden(false)
        .parallelism(super::workers::parallelism(threads))
        .process_read_dir({
            let root_key = root_key.to_string();
            move |_depth, _path, _state, children| {
                if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    children.clear();
                    return;
                }
                for entry in children.iter_mut().flatten() {
                    if let Some(child_path) = entry.read_children_path.as_ref() {
                        if is_root_index_prune_child(&root_key, child_path.as_ref()) {
                            entry.read_children_path = None;
                        }
                    }
                }
            }
        })
        .into_iter()
        .filter_map(|entry| {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return None;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error.to_string())),
            };
            let path = entry.path();
            if let Err(error) = reject_child_read_error(
                &path,
                entry
                    .read_children_error
                    .as_ref()
                    .map(|error| error as &dyn std::fmt::Display),
            ) {
                return Some(Err(error));
            }
            if path == root || path.file_name().is_none() || path.parent().is_none() {
                return None;
            }
            let path = normalize_index_dir(&path);
            if is_root_index_excluded_path(root_key, &path) {
                return None;
            }
            let file_type = entry.file_type();
            let kind = if file_type.is_dir() {
                1
            } else if file_type.is_symlink() {
                2
            } else {
                0
            };
            Some(Ok(ScannedIndexEntry {
                path,
                kind,
                mtime: None,
                size: None,
                allocated_size: None,
                activity: None,
                device: None,
                inode: None,
                link_count: None,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("index scan interrupted".to_string());
    }
    populate_scanned_index_metadata_cancellable(&mut entries, threads, cancel)?;
    Ok(entries)
}

pub(crate) fn populate_scanned_index_metadata_cancellable(
    entries: &mut [ScannedIndexEntry],
    threads: usize,
    cancel: Option<&'static AtomicBool>,
) -> Result<(), String> {
    let populate = |entry: &mut ScannedIndexEntry| {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return;
        }
        let path = if entry.path.contains('%') {
            Cow::Owned(fsx::decode_lossless_path(&entry.path))
        } else {
            Cow::Borrowed(Path::new(&entry.path))
        };
        let metadata = fs::symlink_metadata(path).ok();
        entry.mtime = metadata.as_ref().and_then(metadata_mtime_nanos);
        entry.size = metadata.as_ref().and_then(metadata_size_i64);
        entry.allocated_size = metadata
            .as_ref()
            .and_then(|metadata| i64::try_from(::fsx::metadata::allocated_size(metadata)).ok());
        entry.activity = metadata.as_ref().and_then(metadata_activity_nanos);
        entry.link_count = metadata
            .as_ref()
            .and_then(|metadata| i64::try_from(metadata.nlink()).ok());
        if entry.kind != 1
            && metadata
                .as_ref()
                .is_some_and(|metadata| metadata.nlink() > 1)
        {
            entry.device = metadata
                .as_ref()
                .and_then(|metadata| i64::try_from(metadata.dev()).ok());
            entry.inode = metadata
                .as_ref()
                .and_then(|metadata| i64::try_from(metadata.ino()).ok());
        } else {
            entry.device = None;
            entry.inode = None;
        }
    };
    if threads == 1 {
        entries.iter_mut().for_each(populate);
    } else if let Some(pool) = super::workers::pool(threads) {
        pool.install(|| entries.par_iter_mut().for_each(populate));
    } else {
        entries.iter_mut().for_each(populate);
    }
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        Err("index scan interrupted".to_string())
    } else {
        Ok(())
    }
}

pub(crate) fn mix_index_fingerprint(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58476d1ce4e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

pub(crate) fn index_fingerprint(entries: &[ScannedIndexEntry]) -> String {
    let mut xor = 0u64;
    let mut sum = 0u64;
    let mut rotated_sum = 0u64;
    for entry in entries {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in entry.path.as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
        hash = (hash ^ entry.kind as u64).wrapping_mul(0x100000001b3);
        let mixed = mix_index_fingerprint(hash);
        xor ^= mixed;
        sum = sum.wrapping_add(mixed);
        rotated_sum = rotated_sum.wrapping_add(mixed.rotate_left((mixed & 63) as u32));
    }
    format!("{}:{xor:016x}:{sum:016x}:{rotated_sum:016x}", entries.len())
}

pub(crate) fn index_fingerprint_key(root_key: &str) -> String {
    format!("root_fingerprint_v1:{:016x}", stable_root_hash(root_key))
}

pub(crate) fn index_fingerprint_value(
    conn: &Connection,
    key: &str,
) -> Result<Option<String>, String> {
    match conn.query_row(
        "SELECT value FROM index_meta WHERE key = ?1",
        [key],
        |row| row.get(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn load_index_id_map(
    conn: &Connection,
    sql: &str,
    query_params: impl rusqlite::Params,
) -> Result<HashMap<String, i64>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(query_params, |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(0)?))
        })
        .map_err(|e| e.to_string())?;
    let mut ids = HashMap::new();
    for row in rows {
        let (value, id) = row.map_err(|e| e.to_string())?;
        ids.insert(value, id);
    }
    Ok(ids)
}

fn load_existing_string_ids(
    conn: &Connection,
    values: &std::collections::HashSet<String>,
) -> Result<HashMap<String, i64>, String> {
    if values.is_empty() {
        return Ok(HashMap::new());
    }
    let values = values.iter().collect::<Vec<_>>();
    let placeholders = std::iter::repeat_n("?", values.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id, value FROM strings WHERE value IN ({placeholders})");
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params_from_iter(values), |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(0)?))
        })
        .map_err(|e| e.to_string())?;
    rows.map(|row| row.map_err(|e| e.to_string()))
        .collect::<Result<HashMap<_, _>, _>>()
}

pub(crate) fn next_index_id(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(
        &format!("SELECT COALESCE(MAX(id), 0) + 1 FROM {table}"),
        [],
        |row| row.get(0),
    )
    .map_err(|e| e.to_string())
}

pub(crate) fn metadata_mtime_nanos(metadata: &fs::Metadata) -> Option<i64> {
    metadata.modified().ok().and_then(system_time_to_unix_nanos)
}

pub(crate) fn metadata_activity_nanos(metadata: &fs::Metadata) -> Option<i64> {
    let modified = metadata_mtime_nanos(metadata);
    let created = metadata.created().ok().and_then(system_time_to_unix_nanos);
    match (modified, created) {
        (Some(m), Some(c)) => Some(m.max(c)),
        (Some(m), None) => Some(m),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

pub(crate) fn metadata_size_i64(metadata: &fs::Metadata) -> Option<i64> {
    i64::try_from(metadata.len()).ok()
}

pub(crate) fn ensure_index_id(
    stmt: &mut rusqlite::Statement<'_>,
    ids: &mut HashMap<String, i64>,
    next_id: &mut i64,
    value: &str,
) -> Result<i64, String> {
    if let Some(id) = ids.get(value) {
        return Ok(*id);
    }
    let id = *next_id;
    *next_id = next_id
        .checked_add(1)
        .ok_or_else(|| "index ID space exhausted".to_string())?;
    stmt.execute(params![id, value])
        .map_err(|e| e.to_string())?;
    ids.insert(value.to_string(), id);
    Ok(id)
}

pub(crate) fn insert_index_entries(
    tx: &rusqlite::Transaction<'_>,
    entries: &[PendingIndexEntry],
) -> Result<(), String> {
    for batch in entries.chunks(INDEX_INSERT_BATCH_SIZE) {
        let values = std::iter::repeat_n("(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO entries(dir_id, name_id, kind, mtime, size, allocated_size, activity,
                                 device, inode, link_count) VALUES {values}
             ON CONFLICT(dir_id, name_id, kind) DO UPDATE SET
                 mtime=excluded.mtime,
                 size=excluded.size,
                 allocated_size=excluded.allocated_size,
                 activity=excluded.activity,
                 device=excluded.device,
                 inode=excluded.inode,
                 link_count=excluded.link_count"
        );
        let mut params = Vec::<rusqlite::types::Value>::with_capacity(batch.len() * 10);
        for entry in batch {
            params.push(entry.dir_id.into());
            params.push(entry.name_id.into());
            params.push(entry.kind.into());
            params.push(entry.mtime.into());
            params.push(entry.size.into());
            params.push(entry.allocated_size.into());
            params.push(entry.activity.into());
            params.push(entry.device.into());
            params.push(entry.inode.into());
            params.push(entry.link_count.into());
        }
        tx.prepare_cached(&sql)
            .map_err(|e| e.to_string())?
            .execute(params_from_iter(params))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn load_existing_index_entries(
    conn: &Connection,
    root_key: &str,
    root_prefix: &str,
    root_prefix_end: &str,
) -> Result<Vec<ExistingIndexEntry<'static>>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.dir_id, e.name_id, d.path, s.value, e.kind, e.mtime, e.size,
                    e.allocated_size, e.activity, e.device, e.inode, e.link_count
                 FROM dirs d
             CROSS JOIN entries e INDEXED BY idx_entries_dir ON e.dir_id = d.id
             JOIN strings s ON e.name_id = s.id
             WHERE d.path = ?1 OR (d.path >= ?2 AND d.path < ?3)",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![root_key, root_prefix, root_prefix_end], |row| {
            let dir_id = row.get(0)?;
            let name_id = row.get(1)?;
            let dir: String = row.get(2)?;
            let name: String = row.get(3)?;
            let kind = row.get(4)?;
            let mtime = row.get(5)?;
            let size = row.get(6)?;
            let allocated_size = row.get(7)?;
            let activity = row.get(8)?;
            let device = row.get(9)?;
            let inode = row.get(10)?;
            let link_count = row.get(11)?;
            let path = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            Ok(ExistingIndexEntry {
                dir_id,
                name_id,
                path: Cow::Owned(path),
                kind,
                mtime,
                size,
                allocated_size,
                activity,
                device,
                inode,
                link_count,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

pub(crate) fn compare_index_entry(
    path_a: &str,
    kind_a: i64,
    path_b: &str,
    kind_b: i64,
) -> CmpOrdering {
    path_a.cmp(path_b).then_with(|| kind_a.cmp(&kind_b))
}

pub(crate) fn sort_and_dedup_scanned_index_entries(entries: &mut Vec<ScannedIndexEntry>) {
    entries.par_sort_unstable_by(|a, b| compare_index_entry(&a.path, a.kind, &b.path, b.kind));
    entries.dedup_by(|current, previous| {
        current.path == previous.path && current.kind == previous.kind
    });
}

pub(crate) fn diff_index_entries(
    scanned: &[ScannedIndexEntry],
    existing: &[ExistingIndexEntry<'_>],
) -> (Vec<usize>, Vec<usize>, Vec<(usize, usize)>) {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let (mut scan_idx, mut existing_idx) = (0, 0);

    while scan_idx < scanned.len() && existing_idx < existing.len() {
        match compare_index_entry(
            &scanned[scan_idx].path,
            scanned[scan_idx].kind,
            &existing[existing_idx].path,
            existing[existing_idx].kind,
        ) {
            CmpOrdering::Less => {
                added.push(scan_idx);
                scan_idx += 1;
            }
            CmpOrdering::Greater => {
                removed.push(existing_idx);
                existing_idx += 1;
            }
            CmpOrdering::Equal => {
                if scanned[scan_idx].mtime != existing[existing_idx].mtime
                    || scanned[scan_idx].size != existing[existing_idx].size
                    || scanned[scan_idx].allocated_size != existing[existing_idx].allocated_size
                    || scanned[scan_idx].activity != existing[existing_idx].activity
                    || scanned[scan_idx].device != existing[existing_idx].device
                    || scanned[scan_idx].inode != existing[existing_idx].inode
                    || scanned[scan_idx].link_count != existing[existing_idx].link_count
                {
                    updated.push((existing_idx, scan_idx));
                }
                scan_idx += 1;
                existing_idx += 1;
            }
        }
    }
    added.extend(scan_idx..scanned.len());
    removed.extend(existing_idx..existing.len());
    (removed, added, updated)
}

pub(crate) fn delete_index_entries(
    tx: &rusqlite::Transaction<'_>,
    existing: &[ExistingIndexEntry<'_>],
    entry_indices: &[usize],
) -> Result<(), String> {
    for batch in entry_indices.chunks(INDEX_INSERT_BATCH_SIZE) {
        let placeholders = std::iter::repeat_n("(?, ?, ?)", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM entries WHERE (dir_id, name_id, kind) IN ({placeholders})");
        let values = batch.iter().flat_map(|index| {
            let entry = &existing[*index];
            [entry.dir_id, entry.name_id, entry.kind]
        });
        tx.prepare_cached(&sql)
            .map_err(|e| e.to_string())?
            .execute(params_from_iter(values))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn update_index_entries_metadata(
    tx: &rusqlite::Transaction<'_>,
    existing: &[ExistingIndexEntry<'_>],
    scanned: &[ScannedIndexEntry],
    updates: &[(usize, usize)],
) -> Result<(), String> {
    let mut stmt = tx
        .prepare_cached(
            "UPDATE entries
             SET mtime = ?1, size = ?2, allocated_size = ?3, activity = ?4,
                 device = ?5, inode = ?6, link_count = ?7
             WHERE dir_id = ?8 AND name_id = ?9 AND kind = ?10",
        )
        .map_err(|e| e.to_string())?;
    for &(existing_idx, scanned_idx) in updates {
        let old = &existing[existing_idx];
        let new = &scanned[scanned_idx];
        stmt.execute(params![
            new.mtime,
            new.size,
            new.allocated_size,
            new.activity,
            new.device,
            new.inode,
            new.link_count,
            old.dir_id,
            old.name_id,
            old.kind,
        ])
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn apply_index_metadata_updates(
    existing: &mut [ExistingIndexEntry<'_>],
    scanned: &[ScannedIndexEntry],
    updates: &[(usize, usize)],
) {
    for &(existing_idx, scanned_idx) in updates {
        let old = &mut existing[existing_idx];
        let new = &scanned[scanned_idx];
        old.mtime = new.mtime;
        old.size = new.size;
        old.allocated_size = new.allocated_size;
        old.activity = new.activity;
        old.device = new.device;
        old.inode = new.inode;
        old.link_count = new.link_count;
    }
}

pub(crate) fn get_or_insert_index_id(
    select: &mut rusqlite::Statement<'_>,
    insert: &mut rusqlite::Statement<'_>,
    cache: &mut HashMap<String, i64>,
    value: &str,
) -> Result<i64, String> {
    if let Some(id) = cache.get(value) {
        return Ok(*id);
    }
    let id = match select.query_row([value], |row| row.get(0)) {
        Ok(id) => id,
        Err(rusqlite::Error::QueryReturnedNoRows) => insert
            .query_row([value], |row| row.get(0))
            .map_err(|e| e.to_string())?,
        Err(error) => return Err(error.to_string()),
    };
    cache.insert(value.to_string(), id);
    Ok(id)
}

pub(crate) fn incremental_change_limit(existing_len: usize, scanned_len: usize) -> usize {
    let proportional = existing_len
        .max(scanned_len)
        .div_ceil(INDEX_INCREMENTAL_CHANGE_DIVISOR);
    proportional.clamp(1_024, INDEX_INCREMENTAL_MAX_CHANGES)
}

pub(crate) fn write_manifest_from_existing(
    root_key: &str,
    fingerprint: &str,
    entries: &[ExistingIndexEntry<'_>],
) -> Result<(), String> {
    let (path, tmp, mut writer) = begin_index_manifest(root_key, fingerprint, entries.len())?;
    for entry in entries {
        write_index_manifest_record(
            &mut writer,
            &PendingIndexEntry {
                dir_id: entry.dir_id,
                name_id: entry.name_id,
                kind: entry.kind,
                mtime: entry.mtime,
                size: entry.size,
                allocated_size: entry.allocated_size,
                activity: entry.activity,
                device: entry.device,
                inode: entry.inode,
                link_count: entry.link_count,
            },
            &entry.path,
        )?;
    }
    finish_index_manifest(path, tmp, writer)
}

pub(crate) fn write_manifest_from_scanned(
    root_key: &str,
    fingerprint: &str,
    scanned: &[ScannedIndexEntry],
    entries: &[PendingIndexEntry],
) -> Result<(), String> {
    if scanned.len() != entries.len() {
        return Err("manifest entry count mismatch".to_string());
    }
    let (path, tmp, mut writer) = begin_index_manifest(root_key, fingerprint, scanned.len())?;
    for (scanned, entry) in scanned.iter().zip(entries) {
        write_index_manifest_record(&mut writer, entry, &scanned.path)?;
    }
    finish_index_manifest(path, tmp, writer)
}

pub(crate) fn write_incremental_manifest(
    root_key: &str,
    fingerprint: &str,
    scanned: &[ScannedIndexEntry],
    existing: &[ExistingIndexEntry<'_>],
    added: &HashMap<usize, PendingIndexEntry>,
) -> Result<(), String> {
    let (path, tmp, mut writer) = begin_index_manifest(root_key, fingerprint, scanned.len())?;
    let mut existing_idx = 0;
    for (scan_idx, scanned_entry) in scanned.iter().enumerate() {
        if let Some(entry) = added.get(&scan_idx) {
            write_index_manifest_record(&mut writer, entry, &scanned_entry.path)?;
            continue;
        }
        while existing_idx < existing.len()
            && compare_index_entry(
                &existing[existing_idx].path,
                existing[existing_idx].kind,
                &scanned_entry.path,
                scanned_entry.kind,
            ) == CmpOrdering::Less
        {
            existing_idx += 1;
        }
        let Some(entry) = existing.get(existing_idx) else {
            return Err("incremental manifest lost an unchanged entry".to_string());
        };
        if compare_index_entry(
            &entry.path,
            entry.kind,
            &scanned_entry.path,
            scanned_entry.kind,
        ) != CmpOrdering::Equal
        {
            return Err("incremental manifest entry mismatch".to_string());
        }
        write_index_manifest_record(
            &mut writer,
            &PendingIndexEntry {
                dir_id: entry.dir_id,
                name_id: entry.name_id,
                kind: entry.kind,
                mtime: entry.mtime,
                size: entry.size,
                allocated_size: entry.allocated_size,
                activity: entry.activity,
                device: entry.device,
                inode: entry.inode,
                link_count: entry.link_count,
            },
            &scanned_entry.path,
        )?;
        existing_idx += 1;
    }
    finish_index_manifest(path, tmp, writer)
}

pub(crate) fn read_index_delta(
    root_key: &str,
    fingerprint: &str,
) -> Result<BTreeMap<(String, i64), IndexDeltaEntry>, String> {
    let Some(path) = index_delta_path(root_key) else {
        return Ok(BTreeMap::new());
    };
    let Ok(bytes) = fs::read(path) else {
        return Ok(BTreeMap::new());
    };
    if bytes.get(..8) != Some(INDEX_DELTA_MAGIC) {
        return Err("unsupported index delta version".to_string());
    }
    let mut cursor = 8;
    if read_string_bytes(&bytes, &mut cursor)? != root_key {
        return Err("index delta root mismatch".to_string());
    }
    if read_string_bytes(&bytes, &mut cursor)? != fingerprint {
        return Err("index delta fingerprint mismatch".to_string());
    }
    let _all_count = read_u64_bytes(&bytes, &mut cursor)?;
    let _file_count = read_u64_bytes(&bytes, &mut cursor)?;
    let _dir_count = read_u64_bytes(&bytes, &mut cursor)?;
    let count = usize::try_from(read_u64_bytes(&bytes, &mut cursor)?)
        .map_err(|_| "index delta is too large".to_string())?;
    let mut entries = BTreeMap::new();
    for _ in 0..count {
        let added = *bytes
            .get(cursor)
            .ok_or_else(|| "truncated index delta action".to_string())?
            != 0;
        cursor += 1;
        let dir_id = read_i64_bytes(&bytes, &mut cursor)?;
        let name_id = read_i64_bytes(&bytes, &mut cursor)?;
        let kind = *bytes
            .get(cursor)
            .ok_or_else(|| "truncated index delta kind".to_string())? as i64;
        cursor += 1;
        let path = read_string_bytes(&bytes, &mut cursor)?.to_string();
        entries.insert(
            (path.clone(), kind),
            IndexDeltaEntry {
                added,
                dir_id,
                name_id,
                kind,
                path,
            },
        );
    }
    Ok(entries)
}

pub(crate) fn write_index_delta(
    root_key: &str,
    fingerprint: &str,
    scanned: &[ScannedIndexEntry],
    entries: &BTreeMap<(String, i64), IndexDeltaEntry>,
) -> Result<(), String> {
    if entries.is_empty() {
        remove_index_delta(root_key);
        return Ok(());
    }
    let path = index_delta_path(root_key)
        .ok_or_else(|| "unable to resolve index delta path".to_string())?;
    let tmp = path.with_extension(format!("delta.tmp.{}", unique_temp_tag()));
    let mut writer =
        BufWriter::with_capacity(1024 * 1024, File::create(&tmp).map_err(|e| e.to_string())?);
    writer
        .write_all(INDEX_DELTA_MAGIC)
        .map_err(|e| e.to_string())?;
    write_len_prefixed(&mut writer, root_key)?;
    write_len_prefixed(&mut writer, fingerprint)?;
    let all_count = scanned.len() as u64;
    let file_count = scanned.iter().filter(|entry| entry.kind == 0).count() as u64;
    let dir_count = scanned.iter().filter(|entry| entry.kind == 1).count() as u64;
    for value in [all_count, file_count, dir_count, entries.len() as u64] {
        writer
            .write_all(&value.to_le_bytes())
            .map_err(|e| e.to_string())?;
    }
    for entry in entries.values() {
        writer
            .write_all(&[u8::from(entry.added)])
            .map_err(|e| e.to_string())?;
        writer
            .write_all(&entry.dir_id.to_le_bytes())
            .map_err(|e| e.to_string())?;
        writer
            .write_all(&entry.name_id.to_le_bytes())
            .map_err(|e| e.to_string())?;
        writer
            .write_all(&[entry.kind as u8])
            .map_err(|e| e.to_string())?;
        write_len_prefixed(&mut writer, &entry.path)?;
    }
    writer.flush().map_err(|e| e.to_string())?;
    writer.get_ref().sync_all().map_err(|e| e.to_string())?;
    drop(writer);
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    sync_parent_dir(&path)
}

pub(crate) fn remove_index_delta(root_key: &str) {
    if let Some(path) = index_delta_path(root_key) {
        let _ = fs::remove_file(path);
    }
}

fn remove_index_sidecars_for_roots(roots: &[String]) {
    for root in roots {
        for path in [
            index_state_path(root, "stamp"),
            index_snapshot_path(root),
            index_manifest_path(root),
            index_delta_path(root),
            index_state_path(root, "snapshot"),
        ]
        .into_iter()
        .flatten()
        {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn rebuild_index_base_sidecars(
    conn: &Connection,
    root_key: &str,
    fingerprint: &str,
) -> Result<(), String> {
    // Readers may briefly see the old base without a delta, but never a new base
    // combined with tombstones that were defined against the old base.
    remove_index_delta(root_key);
    write_index_snapshot_from_db(conn, root_key)?;
    let root_prefix = index_path_prefix(root_key);
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let mut entries = load_existing_index_entries(conn, root_key, &root_prefix, &root_prefix_end)?;
    entries.par_sort_unstable_by(|a, b| compare_index_entry(&a.path, a.kind, &b.path, b.kind));
    write_manifest_from_existing(root_key, fingerprint, &entries)?;
    Ok(())
}

pub(crate) fn index_sidecars_are_current(root_key: &str, fingerprint: &str) -> bool {
    let snapshot_exists = index_snapshot_path(root_key).is_some_and(|path| path.is_file());
    if !snapshot_exists {
        return false;
    }
    let Ok(Some(mmap)) = open_index_manifest(root_key) else {
        return false;
    };
    if parse_index_manifest(&mmap, root_key, fingerprint).is_err() {
        return false;
    }
    read_index_delta(root_key, fingerprint).is_ok()
}

pub(crate) fn normalize_index_dir(path: &Path) -> String {
    let mut out = fsx::encode_lossless_path(path);
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

fn split_index_entry_path(path: &str) -> Option<(&str, &str)> {
    let (parent, name) = path.rsplit_once('/')?;
    let parent = if parent.is_empty() { "/" } else { parent };
    (!name.is_empty()).then_some((parent, name))
}

pub(crate) fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

pub(crate) fn normalize_index_root_path(root_raw: &Path) -> Result<String, String> {
    let expanded = expand_home_path_os(root_raw);
    if let Ok(canonical) = fs::canonicalize(&expanded) {
        return Ok(normalize_index_dir(&canonical));
    }
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .map_err(|e| e.to_string())?
            .join(expanded)
    };
    Ok(normalize_index_dir(&normalize_lexical_path(&absolute)))
}

fn expand_home_path_os(path: &Path) -> PathBuf {
    let Some(home) = env::var_os("HOME") else {
        return path.to_path_buf();
    };
    #[cfg(unix)]
    let bytes = path.as_os_str().as_bytes();
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().as_bytes();
    if bytes == b"~" {
        return PathBuf::from(home);
    }
    if let Some(rest) = bytes.strip_prefix(b"~/") {
        #[cfg(unix)]
        {
            let mut expanded = PathBuf::from(home);
            expanded.push(std::ffi::OsString::from_vec(rest.to_vec()));
            return expanded;
        }
        #[cfg(not(unix))]
        {
            return PathBuf::from(home).join(String::from_utf8_lossy(rest).as_ref());
        }
    }
    path.to_path_buf()
}

pub(crate) fn sql_like_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn sql_like_from_wildcard(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                match next {
                    '%' | '_' | '\\' => {
                        out.push('\\');
                        out.push(next);
                    }
                    '*' => out.push('*'),
                    _ => {
                        out.push('\\');
                        out.push(next);
                    }
                }
            } else {
                out.push('\\');
            }
            continue;
        }
        match ch {
            '*' => out.push('%'),
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn fts_trigram_query(raw: &str) -> Option<String> {
    if raw.contains('*') || raw.contains('/') || is_wrapped_quote(raw) || raw.chars().count() < 3 {
        return None;
    }
    Some(format!("\"{}\"", raw.replace('"', "\"\"")))
}

pub(crate) fn sql_prefilter_for_term(
    raw: &str,
    regex_mode: bool,
    case_sensitive: bool,
    force_full: bool,
    field_expr: &str,
    fts_ready: bool,
) -> Option<(String, Vec<String>)> {
    if regex_mode || case_sensitive {
        return None;
    }
    if !raw.is_empty() && raw.bytes().all(|byte| byte == b'*') {
        return None;
    }

    let mut params = Vec::new();
    if is_wrapped_quote(raw) {
        let mut inner = raw[1..raw.len() - 1].to_string();
        inner = inner.trim_start_matches('/').to_string();
        if inner != "/" {
            inner = inner.trim_end_matches('/').to_string();
        }
        if inner.contains('*') {
            params.push(sql_like_from_wildcard(&inner.to_lowercase()));
            return Some((format!("{} LIKE ? ESCAPE '\\'", field_expr), params));
        }
        params.push(inner.to_lowercase());
        return Some((format!("{} = ?", field_expr), params));
    }

    if force_full {
        if !raw.contains('/') {
            if fts_ready {
                if let Some(query) = fts_trigram_query(raw) {
                    params.push(query.clone());
                    params.push(query);
                    return Some((
                        "(e.name_id IN (SELECT rowid FROM strings_fts WHERE strings_fts MATCH ?) OR e.dir_id IN (SELECT rowid FROM dirs_fts WHERE dirs_fts MATCH ?))".to_string(),
                        params,
                    ));
                }
            }
            let lowered = raw.to_lowercase();
            if raw.contains('*') {
                let like = sql_like_from_wildcard(&lowered);
                params.push(like.clone());
                params.push(like);
                return Some((
                    "(e.name_id IN (SELECT id FROM strings WHERE lower(value) LIKE ? ESCAPE '\\') OR e.dir_id IN (SELECT id FROM dirs WHERE lower(path) LIKE ? ESCAPE '\\'))".to_string(),
                    params,
                ));
            }
            let narrowed = if raw.starts_with('/') && raw.ends_with('/') && raw.len() > 1 {
                lowered[1..lowered.len() - 1].to_string()
            } else if raw.starts_with('/') && raw.len() > 1 {
                lowered.trim_start_matches('/').to_string()
            } else if raw != "/" && raw.ends_with('/') {
                lowered.trim_end_matches('/').to_string()
            } else {
                lowered
            };
            params.push(narrowed.clone());
            params.push(narrowed);
            return Some((
                "(e.name_id IN (SELECT id FROM strings WHERE instr(lower(value), ?) > 0) OR e.dir_id IN (SELECT id FROM dirs WHERE instr(lower(path), ?) > 0))".to_string(),
                params,
            ));
        }

        if raw.contains('*') {
            params.push(sql_like_from_wildcard(&raw.to_lowercase()));
            return Some((format!("{} LIKE ? ESCAPE '\\'", field_expr), params));
        }
        let lowered = raw.to_lowercase();
        if raw.starts_with('/') && raw.ends_with('/') && raw.len() > 1 {
            params.push(lowered[1..lowered.len() - 1].to_string());
        } else if raw.starts_with('/') && raw.len() > 1 {
            params.push(lowered.trim_start_matches('/').to_string());
        } else if raw != "/" && raw.ends_with('/') {
            params.push(lowered.trim_end_matches('/').to_string());
        } else {
            params.push(lowered);
        }
        return Some((format!("instr({}, ?) > 0", field_expr), params));
    }

    if raw.starts_with('/') && raw.ends_with('/') {
        params.push(raw[1..raw.len() - 1].to_lowercase());
        return Some((
            "e.name_id IN (SELECT id FROM strings WHERE lower(value) = ?)".to_string(),
            params,
        ));
    }
    if let Some(stripped) = raw.strip_prefix('/') {
        params.push(format!("{}%", sql_like_escape(&stripped.to_lowercase())));
        return Some((
            "e.name_id IN (SELECT id FROM strings WHERE lower(value) LIKE ? ESCAPE '\\')"
                .to_string(),
            params,
        ));
    }
    if raw != "/" && raw.ends_with('/') {
        params.push(format!(
            "%{}",
            sql_like_escape(&raw[..raw.len() - 1].to_lowercase())
        ));
        return Some((
            "e.name_id IN (SELECT id FROM strings WHERE lower(value) LIKE ? ESCAPE '\\')"
                .to_string(),
            params,
        ));
    }
    if raw.contains('*') {
        params.push(sql_like_from_wildcard(&raw.to_lowercase()));
        return Some((
            "e.name_id IN (SELECT id FROM strings WHERE lower(value) LIKE ? ESCAPE '\\')"
                .to_string(),
            params,
        ));
    }

    if fts_ready {
        if let Some(query) = fts_trigram_query(raw) {
            params.push(query);
            return Some((
                "e.name_id IN (SELECT rowid FROM strings_fts WHERE strings_fts MATCH ?)"
                    .to_string(),
                params,
            ));
        }
    }
    params.push(raw.to_lowercase());
    Some((
        "e.name_id IN (SELECT id FROM strings WHERE instr(lower(value), ?) > 0)".to_string(),
        params,
    ))
}

pub(crate) fn index_path_prefix(path: &str) -> String {
    if path == "/" {
        "/".to_string()
    } else {
        format!("{}/", path.trim_end_matches('/'))
    }
}

pub(crate) fn is_root_index_prune_child(root_key: &str, path: &Path) -> bool {
    (root_key == "/" && matches!(path.to_str(), Some("/proc" | "/sys" | "/dev" | "/run")))
        || is_unearth_internal_index_path(&normalize_index_dir(path))
}

pub(crate) fn is_root_index_excluded_path(root_key: &str, path: &str) -> bool {
    is_unearth_internal_index_path(path)
        || (root_key == "/"
            && ["/proc", "/sys", "/dev", "/run"]
                .iter()
                .any(|prefix| path == *prefix || path.starts_with(&format!("{}/", prefix))))
}

pub(crate) fn purge_index_root(root_raw: &str) -> Result<(), String> {
    purge_index_root_path(Path::new(&expand_home_path(root_raw)))
}

pub(crate) fn purge_index_root_path(root_raw: &Path) -> Result<(), String> {
    let root_key = normalize_index_root_path(root_raw)?;
    let root_prefix = index_path_prefix(&root_key);
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let mut conn = initialize_index_db()?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let active_watch_roots = tx
        .prepare("SELECT root FROM watch_state WHERE online = 1 AND status != 'stopped'")
        .map_err(|e| e.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    if active_watch_roots.iter().any(|watch_root| {
        watch_root == &root_key
            || watch_root.starts_with(&root_prefix)
            || root_key.starts_with(&index_path_prefix(watch_root))
    }) {
        return Err(format!(
            "cannot purge {} while an overlapping live watcher is active; stop the watcher first",
            root_key
        ));
    }
    let mut impacted_roots = Vec::<String>::new();
    {
        let mut stmt = tx
            .prepare("SELECT root FROM indexed_roots")
            .map_err(|e| e.to_string())?;
        let roots = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for root in roots {
            let root = root.map_err(|e| e.to_string())?;
            let indexed_prefix = index_path_prefix(&root);
            if root == root_key
                || root.starts_with(&root_prefix)
                || root_key.starts_with(&indexed_prefix)
            {
                impacted_roots.push(root);
            }
        }
    }
    tx.execute(
        "DELETE FROM entries WHERE dir_id IN (
            SELECT id FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3)
        )",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM indexed_roots WHERE root = ?1 OR (root >= ?2 AND root < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM watch_state WHERE root = ?1 OR (root >= ?2 AND root < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    for impacted_root in &impacted_roots {
        tx.execute("DELETE FROM indexed_roots WHERE root = ?1", [impacted_root])
            .map_err(|e| e.to_string())?;
    }
    tx.execute(
        "DELETE FROM strings WHERE id NOT IN (
            SELECT DISTINCT name_id FROM entries
        )",
        [],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    let mut state_roots = impacted_roots;
    state_roots.push(root_key);
    state_roots.sort();
    state_roots.dedup();
    for state_root in state_roots {
        if let Some(stamp_path) = index_state_path(&state_root, "stamp") {
            let _ = fs::remove_file(stamp_path);
        }
        // Leave refresh locks alone. A concurrent refresh owns its lock and
        // will remove it; deleting it here could allow two writers to race.
        if let Some(snapshot_path) = index_snapshot_path(&state_root) {
            let _ = fs::remove_file(snapshot_path);
        }
        if let Some(manifest_path) = index_manifest_path(&state_root) {
            let _ = fs::remove_file(manifest_path);
        }
        if let Some(delta_path) = index_delta_path(&state_root) {
            let _ = fs::remove_file(delta_path);
        }
        if let Some(legacy_path) = index_state_path(&state_root, "snapshot") {
            let _ = fs::remove_file(legacy_path);
        }
    }
    Ok(())
}

pub(crate) fn refresh_index_root(root_raw: &str, opts: &Options) -> Result<(), String> {
    refresh_index_root_cancellable(root_raw, opts, None)
}

pub(crate) fn refresh_index_root_path(root_raw: &Path, opts: &Options) -> Result<(), String> {
    refresh_index_root_path_cancellable(root_raw, opts, None)
}

pub(crate) fn refresh_index_root_cancellable(
    root_raw: &str,
    opts: &Options,
    cancel: Option<&'static AtomicBool>,
) -> Result<(), String> {
    refresh_index_root_path_cancellable(Path::new(&expand_home_path(root_raw)), opts, cancel)
}

fn refresh_index_root_path_cancellable(
    root_raw: &Path,
    opts: &Options,
    cancel: Option<&'static AtomicBool>,
) -> Result<(), String> {
    let root = fs::canonicalize(expand_home_path_os(root_raw)).map_err(|e| e.to_string())?;
    if !root.is_dir() {
        return Err(format!(
            "--index-refresh target '{}' is not a directory",
            root_raw.display()
        ));
    }
    let root_key = normalize_index_dir(&root);
    let lock_path = index_state_path(&root_key, "lock")
        .ok_or_else(|| "unable to resolve index refresh lock path".to_string())?;
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _refresh_lock_file = acquire_index_refresh_lock(&lock_path)
        .ok_or_else(|| format!("an index refresh is already running for {}", root_key))?;
    let _refresh_lock = RefreshLockGuard {
        path: lock_path,
        owner: refresh_lock_owner(),
    };
    let scan_threads = if root_prefers_single_thread(&root) {
        1
    } else {
        opts.threads_override.max(1)
    };
    let scanned_entries = scan_index_root_cancellable(&root, &root_key, scan_threads, cancel)?;
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("index scan interrupted".to_string());
    }
    refresh_index_root_from_scan(&root_key, scanned_entries)
}

pub(crate) fn refresh_index_root_from_scan(
    root_key: &str,
    mut scanned_entries: Vec<ScannedIndexEntry>,
) -> Result<(), String> {
    let root_prefix = index_path_prefix(root_key);
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    sort_and_dedup_scanned_index_entries(&mut scanned_entries);
    let fingerprint = index_fingerprint(&scanned_entries);
    let fingerprint_key = index_fingerprint_key(root_key);
    let mut conn = initialize_index_db()?;
    let superseded_roots = {
        let mut stmt = conn
            .prepare(
                "SELECT root FROM indexed_roots
                 WHERE root >= ?1 AND root < ?2 AND root != ?3",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![root_prefix, root_prefix_end, root_key], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let previous_fingerprint = index_fingerprint_value(&tx, &fingerprint_key)?;
    let manifest = open_index_manifest(root_key).ok().flatten();
    let mut existing_entries = if let (Some(manifest), Some(previous_fingerprint)) =
        (manifest.as_ref(), previous_fingerprint.as_deref())
    {
        match parse_index_manifest(manifest, root_key, previous_fingerprint) {
            Ok(entries) => entries,
            Err(_) => load_existing_index_entries(&tx, root_key, &root_prefix, &root_prefix_end)?,
        }
    } else {
        load_existing_index_entries(&tx, root_key, &root_prefix, &root_prefix_end)?
    };
    existing_entries
        .par_sort_unstable_by(|a, b| compare_index_entry(&a.path, a.kind, &b.path, b.kind));
    let (removed_entry_indices, added_entry_indices, updated_entry_indices) =
        diff_index_entries(&scanned_entries, &existing_entries);
    let changed_count = removed_entry_indices.len() + added_entry_indices.len();
    let incremental_limit = incremental_change_limit(existing_entries.len(), scanned_entries.len());
    if !existing_entries.is_empty() && changed_count <= incremental_limit {
        delete_index_entries(&tx, &existing_entries, &removed_entry_indices)?;
        update_index_entries_metadata(
            &tx,
            &existing_entries,
            &scanned_entries,
            &updated_entry_indices,
        )?;
        apply_index_metadata_updates(
            &mut existing_entries,
            &scanned_entries,
            &updated_entry_indices,
        );

        let mut select_string = tx
            .prepare("SELECT id FROM strings WHERE value = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_string = tx
            .prepare("INSERT INTO strings(value) VALUES (?1) RETURNING id")
            .map_err(|e| e.to_string())?;
        let mut select_dir = tx
            .prepare("SELECT id FROM dirs WHERE path = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_dir = tx
            .prepare("INSERT INTO dirs(path) VALUES (?1) RETURNING id")
            .map_err(|e| e.to_string())?;
        let mut string_ids = HashMap::new();
        let mut dir_ids = HashMap::new();
        let mut pending_entries = Vec::with_capacity(added_entry_indices.len());
        let mut added_entries = HashMap::with_capacity(added_entry_indices.len());

        for &index in &added_entry_indices {
            let scanned = &scanned_entries[index];
            let Some((parent_key, name)) = split_index_entry_path(&scanned.path) else {
                continue;
            };
            let parent_id =
                get_or_insert_index_id(&mut select_dir, &mut insert_dir, &mut dir_ids, parent_key)?;
            let name_id = get_or_insert_index_id(
                &mut select_string,
                &mut insert_string,
                &mut string_ids,
                name,
            )?;
            let pending = PendingIndexEntry {
                dir_id: parent_id,
                name_id,
                kind: scanned.kind,
                mtime: scanned.mtime,
                size: scanned.size,
                allocated_size: scanned.allocated_size,
                activity: scanned.activity,
                device: scanned.device,
                inode: scanned.inode,
                link_count: scanned.link_count,
            };
            pending_entries.push(pending);
            added_entries.insert(index, pending);
            if scanned.kind == 1 {
                get_or_insert_index_id(
                    &mut select_dir,
                    &mut insert_dir,
                    &mut dir_ids,
                    &scanned.path,
                )?;
            }
        }
        drop(select_string);
        drop(insert_string);
        drop(select_dir);
        drop(insert_dir);
        insert_index_entries(&tx, &pending_entries)?;
        tx.execute(
            "DELETE FROM dirs
             WHERE (path = ?1 OR (path >= ?2 AND path < ?3))
               AND NOT EXISTS (SELECT 1 FROM entries WHERE entries.dir_id = dirs.id)",
            params![root_key, root_prefix, root_prefix_end],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM indexed_roots WHERE root = ?1 OR (root >= ?2 AND root < ?3)",
            params![root_key, root_prefix, root_prefix_end],
        )
        .map_err(|e| e.to_string())?;
        let refreshed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        tx.execute(
            "INSERT INTO indexed_roots(root, refreshed_at) VALUES (?1, ?2)",
            params![root_key, refreshed_at],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES (?1, ?2)",
            params![fingerprint_key, fingerprint],
        )
        .map_err(|e| e.to_string())?;
        if removed_entry_indices.is_empty() && added_entry_indices.is_empty() {
            tx.commit().map_err(|e| e.to_string())?;
            remove_index_sidecars_for_roots(&superseded_roots);
            if !updated_entry_indices.is_empty() {
                write_manifest_from_existing(root_key, &fingerprint, &existing_entries)?;
            } else if !index_sidecars_are_current(root_key, &fingerprint) {
                rebuild_index_base_sidecars(&conn, root_key, &fingerprint)?;
            }
            if let Some(stamp) = index_state_path(root_key, "stamp") {
                write_stamp(&stamp);
            }
            return Ok(());
        }
        let mut delta = if let Some(previous) = previous_fingerprint.as_deref() {
            read_index_delta(root_key, previous)?
        } else {
            BTreeMap::new()
        };
        for index in &removed_entry_indices {
            let entry = &existing_entries[*index];
            let key = (entry.path.to_string(), entry.kind);
            if delta.get(&key).is_some_and(|record| record.added) {
                delta.remove(&key);
            } else {
                delta.insert(
                    key,
                    IndexDeltaEntry {
                        added: false,
                        dir_id: entry.dir_id,
                        name_id: entry.name_id,
                        kind: entry.kind,
                        path: entry.path.to_string(),
                    },
                );
            }
        }
        for (&index, entry) in &added_entries {
            let scanned = &scanned_entries[index];
            let key = (scanned.path.clone(), scanned.kind);
            if delta.get(&key).is_some_and(|record| !record.added) {
                delta.remove(&key);
            } else {
                delta.insert(
                    key,
                    IndexDeltaEntry {
                        added: true,
                        dir_id: entry.dir_id,
                        name_id: entry.name_id,
                        kind: entry.kind,
                        path: scanned.path.clone(),
                    },
                );
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        remove_index_sidecars_for_roots(&superseded_roots);
        write_incremental_manifest(
            root_key,
            &fingerprint,
            &scanned_entries,
            &existing_entries,
            &added_entries,
        )?;
        let snapshot_exists = index_snapshot_path(root_key).is_some_and(|path| path.is_file());
        if !snapshot_exists || delta.len() >= INDEX_DELTA_COMPACT_RECORDS {
            rebuild_index_base_sidecars(&conn, root_key, &fingerprint)?;
        } else {
            write_index_delta(root_key, &fingerprint, &scanned_entries, &delta)?;
        }
        if let Some(stamp) = index_state_path(root_key, "stamp") {
            write_stamp(&stamp);
        }
        return Ok(());
    }
    if existing_entries.is_empty() && scanned_entries.len() <= 10_000 {
        let mut select_dir = tx
            .prepare("SELECT id FROM dirs WHERE path = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_dir = tx
            .prepare("INSERT INTO dirs(path) VALUES (?1) RETURNING id")
            .map_err(|e| e.to_string())?;
        let mut select_string = tx
            .prepare("SELECT id FROM strings WHERE value = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_string = tx
            .prepare("INSERT INTO strings(value) VALUES (?1) RETURNING id")
            .map_err(|e| e.to_string())?;
        let mut dir_ids = HashMap::new();
        let mut string_ids = HashMap::new();
        get_or_insert_index_id(&mut select_dir, &mut insert_dir, &mut dir_ids, root_key)?;
        let mut pending_entries = Vec::with_capacity(scanned_entries.len());
        for scanned in &scanned_entries {
            let Some((parent_key, name)) = split_index_entry_path(&scanned.path) else {
                continue;
            };
            let parent_id =
                get_or_insert_index_id(&mut select_dir, &mut insert_dir, &mut dir_ids, parent_key)?;
            let name_id = get_or_insert_index_id(
                &mut select_string,
                &mut insert_string,
                &mut string_ids,
                name,
            )?;
            if scanned.kind == 1 {
                get_or_insert_index_id(
                    &mut select_dir,
                    &mut insert_dir,
                    &mut dir_ids,
                    &scanned.path,
                )?;
            }
            pending_entries.push(PendingIndexEntry {
                dir_id: parent_id,
                name_id,
                kind: scanned.kind,
                mtime: scanned.mtime,
                size: scanned.size,
                allocated_size: scanned.allocated_size,
                activity: scanned.activity,
                device: scanned.device,
                inode: scanned.inode,
                link_count: scanned.link_count,
            });
        }
        drop(select_dir);
        drop(insert_dir);
        drop(select_string);
        drop(insert_string);
        insert_index_entries(&tx, &pending_entries)?;
        let refreshed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        tx.execute(
            "INSERT OR REPLACE INTO indexed_roots(root, refreshed_at) VALUES (?1, ?2)",
            params![root_key, refreshed_at],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES (?1, ?2)",
            params![fingerprint_key, fingerprint],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        remove_index_sidecars_for_roots(&superseded_roots);
        rebuild_index_base_sidecars(&conn, root_key, &fingerprint)?;
        if let Some(stamp) = index_state_path(root_key, "stamp") {
            write_stamp(&stamp);
        }
        return Ok(());
    }
    drop(existing_entries);

    tx.execute(
        "DELETE FROM entries WHERE dir_id IN (
            SELECT id FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3)
        )",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM indexed_roots WHERE root = ?1 OR (root >= ?2 AND root < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;

    let string_values = scanned_entries
        .iter()
        .filter_map(|scanned| {
            split_index_entry_path(&scanned.path).map(|(_, name)| name.to_owned())
        })
        .collect::<std::collections::HashSet<_>>();
    let mut string_ids = load_existing_string_ids(&tx, &string_values)?;
    let mut dir_ids = load_index_id_map(
        &tx,
        "SELECT id, path FROM dirs
         WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )?;
    let mut next_string_id = next_index_id(&tx, "strings")?;
    let mut next_dir_id = next_index_id(&tx, "dirs")?;
    let mut insert_string = tx
        .prepare("INSERT INTO strings(id, value) VALUES (?1, ?2)")
        .map_err(|e| e.to_string())?;
    let mut insert_dir = tx
        .prepare("INSERT INTO dirs(id, path) VALUES (?1, ?2)")
        .map_err(|e| e.to_string())?;
    ensure_index_id(&mut insert_dir, &mut dir_ids, &mut next_dir_id, root_key)?;
    let mut pending_entries = Vec::<PendingIndexEntry>::new();

    for scanned in &scanned_entries {
        let path_key = &scanned.path;
        let Some((parent_key, name)) = split_index_entry_path(path_key) else {
            continue;
        };
        let parent_id =
            ensure_index_id(&mut insert_dir, &mut dir_ids, &mut next_dir_id, parent_key)?;
        let name_id = ensure_index_id(
            &mut insert_string,
            &mut string_ids,
            &mut next_string_id,
            name,
        )?;
        let kind = scanned.kind;
        pending_entries.push(PendingIndexEntry {
            dir_id: parent_id,
            name_id,
            kind,
            mtime: scanned.mtime,
            size: scanned.size,
            allocated_size: scanned.allocated_size,
            activity: scanned.activity,
            device: scanned.device,
            inode: scanned.inode,
            link_count: scanned.link_count,
        });
        if kind == 1 {
            ensure_index_id(&mut insert_dir, &mut dir_ids, &mut next_dir_id, path_key)?;
        }
    }
    drop(insert_dir);
    drop(insert_string);
    insert_index_entries(&tx, &pending_entries)?;
    drop(dir_ids);
    drop(string_ids);
    tx.execute(
        "DELETE FROM dirs
         WHERE (path = ?1 OR (path >= ?2 AND path < ?3))
           AND NOT EXISTS (SELECT 1 FROM entries WHERE entries.dir_id = dirs.id)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    let refreshed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    tx.execute(
        "INSERT OR REPLACE INTO indexed_roots(root, refreshed_at) VALUES (?1, ?2)",
        params![root_key, refreshed_at],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT OR REPLACE INTO index_meta(key, value) VALUES (?1, ?2)",
        params![fingerprint_key, fingerprint],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    remove_index_sidecars_for_roots(&superseded_roots);
    remove_index_delta(root_key);
    if let Err(error) = write_index_snapshot_from_db(&conn, root_key) {
        if let Some(snapshot_path) = index_snapshot_path(root_key) {
            let _ = fs::remove_file(snapshot_path);
        }
        return Err(format!("failed to build index snapshot: {}", error));
    }
    write_manifest_from_scanned(root_key, &fingerprint, &scanned_entries, &pending_entries)?;
    if let Some(stamp) = index_state_path(root_key, "stamp") {
        write_stamp(&stamp);
    }
    Ok(())
}

pub(crate) fn index_root_is_known(conn: &Connection, root_key: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM indexed_roots
             WHERE root = ?1
             LIMIT 1",
            params![root_key],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(count > 0)
}

pub(crate) fn covering_index_root(
    conn: &Connection,
    root_key: &str,
) -> Result<Option<String>, String> {
    let mut stmt = conn
        .prepare("SELECT root FROM indexed_roots")
        .map_err(|e| e.to_string())?;
    let roots = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut best = None;
    for root in roots {
        let root = root.map_err(|e| e.to_string())?;
        if (root == root_key
            || root == "/"
            || root_key.starts_with(&format!("{}/", root.trim_end_matches('/'))))
            && best
                .as_ref()
                .is_none_or(|current: &String| root.len() > current.len())
        {
            best = Some(root);
        }
    }
    Ok(best)
}

pub(crate) fn spawn_index_refresh(root_key: &str, opts: &Options, force: bool) {
    let Some(stamp_path) = index_state_path(root_key, "stamp") else {
        return;
    };
    if !force && stamp_path.is_file() && !path_age_at_least(&stamp_path, INDEX_REFRESH_MIN_AGE) {
        return;
    }
    let Some(lock_path) = index_state_path(root_key, "lock") else {
        return;
    };
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(contents) = fs::read_to_string(&lock_path) {
        if index_refresh_lock_owner_is_running(&contents) {
            return;
        }
        if contents.trim().is_empty() && refresh_lock_is_recent(&lock_path) {
            return;
        }
    }
    let Ok(exe) = env::current_exe() else {
        let _ = fs::remove_file(lock_path);
        return;
    };
    let mut command = Command::new(exe);
    command
        .arg("--index-refresh")
        .arg(root_key)
        .arg("--threads")
        .arg(opts.threads_override.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.spawn() {
        Ok(_) | Err(_) => {}
    }
}

pub(crate) fn index_refresh_lock_owner_is_running(contents: &str) -> bool {
    let mut fields = contents.split_whitespace();
    let Ok(pid) = fields.next().unwrap_or_default().parse::<u32>() else {
        return false;
    };
    let Ok(owner_exe) = fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    if !env::current_exe().is_ok_and(|current_exe| owner_exe == current_exe) {
        return false;
    }
    fields.next().is_none_or(|expected| {
        refresh_process_starttime(pid).is_some_and(|actual| expected == actual.to_string())
    })
}

fn refresh_process_starttime(pid: u32) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn refresh_lock_owner() -> String {
    let pid = std::process::id();
    format!("{pid} {}", refresh_process_starttime(pid).unwrap_or(-1))
}

pub(crate) fn acquire_index_refresh_lock(lock_path: &Path) -> Option<File> {
    for _ in 0..2 {
        match File::options().write(true).create_new(true).open(lock_path) {
            Ok(mut lock) => {
                if writeln!(lock, "{}", refresh_lock_owner()).is_err() || lock.flush().is_err() {
                    let _ = fs::remove_file(lock_path);
                    return None;
                }
                return Some(lock);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let observed = fs::read_to_string(lock_path).unwrap_or_default();
                if index_refresh_lock_owner_is_running(&observed) {
                    return None;
                }
                if observed.trim().is_empty() && refresh_lock_is_recent(lock_path) {
                    return None;
                }
                if fs::read_to_string(lock_path).unwrap_or_default() != observed {
                    return None;
                }
                if fs::remove_file(lock_path).is_err() {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    None
}

fn refresh_lock_is_recent(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .map(|age| age < INDEX_REFRESH_LOCK_EMPTY_GRACE)
        .unwrap_or(true)
}

struct RefreshLockGuard {
    path: PathBuf,
    owner: String,
}

impl Drop for RefreshLockGuard {
    fn drop(&mut self) {
        let owned = fs::read_to_string(&self.path)
            .map(|contents| contents.trim() == self.owner)
            .unwrap_or(false);
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod scan_completion_tests {
    use super::{normalize_index_dir, reject_child_read_error, scan_index_root_cancellable};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn child_read_errors_abort_index_scans() {
        let source_error = "permission denied";
        let error = reject_child_read_error(
            Path::new("/unreadable"),
            Some(&source_error as &dyn std::fmt::Display),
        )
        .unwrap_err();

        assert!(error.contains("/unreadable"));
        assert!(error.contains("permission denied"));
        assert!(reject_child_read_error(Path::new("/readable"), None).is_ok());
    }

    #[test]
    fn index_scan_rejects_an_unreadable_subtree_when_permissions_are_enforced() {
        let root = std::env::temp_dir().join(format!(
            "unearth-index-unreadable-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let blocked = root.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        fs::write(root.join("visible"), b"visible").unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();

        let permissions_enforced = fs::read_dir(&blocked).is_err();
        let result = scan_index_root_cancellable(&root, &normalize_index_dir(&root), 1, None);

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&root).unwrap();

        if permissions_enforced {
            let error = result.expect_err("unreadable subtree must make the scan incomplete");
            assert!(error.contains("blocked"), "{error}");
        }
    }
}
