use super::*;
use std::sync::OnceLock;
pub(crate) fn snapshot_cache_dir() -> Option<PathBuf> {
    fsx::index::cache_dir().map(|dir| dir.join("snapshots"))
}

pub(crate) fn unearth_cache_dir() -> Option<PathBuf> {
    fsx::index::cache_dir()
}

fn build_unearth_internal_index_paths() -> Vec<(String, String)> {
    let Some(cache_dir) = unearth_cache_dir() else {
        return Vec::new();
    };
    let mut paths = vec![cache_dir.clone()];
    if let Ok(canonical) = cache_dir.canonicalize() {
        if canonical != cache_dir {
            paths.push(canonical);
        }
    }
    let mut internal = paths
        .into_iter()
        .map(|path| {
            let exact = normalize_index_dir(&path);
            let prefix = index_path_prefix(&exact);
            (exact, prefix)
        })
        .collect::<Vec<_>>();

    let Some(database) = index_db_path() else {
        return internal;
    };
    let mut sidecars = vec![database.clone()];
    let mut wal = database.as_os_str().to_os_string();
    wal.push("-wal");
    sidecars.push(PathBuf::from(wal));
    let mut shm = database.as_os_str().to_os_string();
    shm.push("-shm");
    sidecars.push(PathBuf::from(shm));
    if let Some(socket) = query_socket_path() {
        sidecars.push(socket);
    }
    for path in sidecars {
        let exact = fsx::encode_lossless_path(&path);
        // File paths must match exactly; using a directory-style prefix
        // would accidentally exclude unrelated names sharing a prefix.
        internal.push((exact.clone(), format!("{exact}\0")));
    }
    internal
}

pub(crate) fn unearth_internal_index_paths() -> &'static [(String, String)] {
    static PATHS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    PATHS.get_or_init(build_unearth_internal_index_paths)
}

pub(crate) fn is_unearth_internal_index_path(path: &str) -> bool {
    unearth_internal_index_paths()
        .iter()
        .any(|(exact, prefix)| path == exact || path.starts_with(prefix))
}

pub(crate) fn index_db_path() -> Option<PathBuf> {
    fsx::index::database_path()
}

pub(crate) fn legacy_index_db_path() -> Option<PathBuf> {
    fsx::index::legacy_database_path()
}

pub(crate) fn query_socket_path() -> Option<PathBuf> {
    let directory = fsx::index::cache_dir()?.join("index");
    let name = if std::env::var_os("FSX_INDEX_DB").is_some() {
        let database = index_db_path()?;
        format!(
            "fsxd-{:016x}.sock",
            stable_root_hash(&fsx::encode_lossless_path(&database))
        )
    } else {
        QUERY_SOCKET_NAME.to_string()
    };
    Some(directory.join(name))
}

pub(crate) fn watch_state_covers_root(conn: &Connection, root_key: &str) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(
            "SELECT root, watcher_pid, owner_boot_id, owner_starttime, heartbeat
             FROM watch_state
             WHERE status = 'running' AND dirty = 0 AND online = 1",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (root, pid, boot_id, starttime, heartbeat): (
            String,
            Option<i64>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ) = row.map_err(|e| e.to_string())?;
        let heartbeat_fresh = heartbeat.is_some_and(|value| unix_now().saturating_sub(value) <= 15);
        if heartbeat_fresh
            && watcher_owner_is_alive(pid, boot_id.as_deref(), starttime)
            && (root == root_key
                || root == "/"
                || root_key.starts_with(&format!("{}/", root.trim_end_matches('/'))))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn process_starttime(pid: i64) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn watcher_owner_is_alive(pid: Option<i64>, boot_id: Option<&str>, starttime: Option<i64>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    let Some(expected_boot) = boot_id else {
        return false;
    };
    let Some(expected_start) = starttime else {
        return false;
    };
    let current_boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    current_boot == expected_boot && process_starttime(pid) == Some(expected_start)
}

pub(crate) fn print_watch_status() -> Result<(), String> {
    let conn = match open_index_db_writer() {
        Ok(conn) => conn,
        Err(_) => {
            println!("no live watchers");
            return Ok(());
        }
    };
    mark_dead_watch_states(&conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT root, backend, status, generation, last_event, last_reconcile,
                    dirty, online, watcher_pid, heartbeat, COALESCE(error, '')
             FROM watch_state ORDER BY root",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, String>(10)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut any = false;
    for row in rows {
        let (
            root,
            backend,
            status,
            generation,
            last_event,
            last_reconcile,
            dirty,
            online,
            pid,
            heartbeat,
            error,
        ) = row.map_err(|e| e.to_string())?;
        any = true;
        println!(
            "{}\t{}\t{}\tgeneration={}\tlast-event={}\tlast-reconcile={}\tdirty={}\tonline={}\tpid={}\theartbeat={}{}",
            root,
            backend,
            status,
            generation,
            last_event.map_or_else(|| "-".to_string(), |value| value.to_string()),
            last_reconcile.map_or_else(|| "-".to_string(), |value| value.to_string()),
            dirty,
            online,
            pid.map_or_else(|| "-".to_string(), |value| value.to_string()),
            heartbeat.map_or_else(|| "-".to_string(), |value| value.to_string()),
            if error.is_empty() {
                String::new()
            } else {
                format!("\terror={error}")
            }
        );
    }
    if !any {
        println!("no live watchers");
    }
    Ok(())
}

fn mark_dead_watch_states(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT root, watcher_pid, owner_boot_id, owner_starttime, heartbeat
             FROM watch_state WHERE online = 1",
        )
        .map_err(|e| e.to_string())?;
    let stale = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|row| row.ok())
        .filter(|(_, pid, boot, starttime, heartbeat)| {
            let heartbeat_stale =
                heartbeat.is_none_or(|value| unix_now().saturating_sub(value) > 30);
            heartbeat_stale && !watcher_owner_is_alive(*pid, boot.as_deref(), *starttime)
        })
        .map(|(root, _, _, _, _)| root)
        .collect::<Vec<_>>();
    drop(stmt);
    for root in stale {
        conn.execute(
            "UPDATE watch_state SET status = 'stopped', online = 0, dirty = 1,
                 error = COALESCE(error, 'watcher is not running')
             WHERE root = ?1 AND online = 1",
            [&root],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod watch_state_tests {
    use super::*;

    fn state_connection(heartbeat: i64) -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE watch_state (
                    root TEXT PRIMARY KEY,
                    status TEXT NOT NULL,
                    dirty INTEGER NOT NULL,
                    online INTEGER NOT NULL,
                    watcher_pid INTEGER,
                    owner_boot_id TEXT,
                    owner_starttime INTEGER,
                    heartbeat INTEGER,
                    error TEXT
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO watch_state VALUES (
                    '/tmp/root', 'running', 0, 1, 999999999,
                    'wrong-boot', 1, ?1, NULL
                )",
                [heartbeat],
            )
            .unwrap();
        connection
    }

    #[test]
    fn fresh_heartbeat_prevents_false_dead_owner_transition() {
        let connection = state_connection(unix_now());
        mark_dead_watch_states(&connection).unwrap();
        let state: (String, i64, i64) = connection
            .query_row("SELECT status, dirty, online FROM watch_state", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(state, ("running".to_string(), 0, 1));
    }

    #[test]
    fn stale_heartbeat_allows_dead_owner_transition() {
        let connection = state_connection(unix_now() - 31);
        mark_dead_watch_states(&connection).unwrap();
        let state: (String, i64, i64) = connection
            .query_row("SELECT status, dirty, online FROM watch_state", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(state, ("stopped".to_string(), 1, 0));
    }
}

pub(crate) fn index_state_path(root_key: &str, suffix: &str) -> Option<PathBuf> {
    Some(unearth_cache_dir()?.join("index").join(format!(
        "{:016x}.{}",
        stable_root_hash(root_key),
        suffix
    )))
}

pub(crate) fn stable_root_hash(root_key: &str) -> u64 {
    root_key.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

pub(crate) fn index_snapshot_path(root_key: &str) -> Option<PathBuf> {
    Some(
        unearth_cache_dir()?
            .join("index")
            .join(format!("{:016x}.snapshot", stable_root_hash(root_key))),
    )
}

pub(crate) fn index_manifest_path(root_key: &str) -> Option<PathBuf> {
    Some(
        unearth_cache_dir()?
            .join("index")
            .join(format!("{:016x}.manifest", stable_root_hash(root_key))),
    )
}

pub(crate) fn unique_temp_tag() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}.{}", std::process::id(), nanos)
}

pub(crate) fn sync_parent_dir(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "cache file has no parent directory".to_string())?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())
}

pub(crate) fn index_delta_path(root_key: &str) -> Option<PathBuf> {
    Some(
        unearth_cache_dir()?
            .join("index")
            .join(format!("{:016x}.delta", stable_root_hash(root_key))),
    )
}

pub(crate) fn write_len_prefixed(writer: &mut impl Write, value: &str) -> Result<(), String> {
    let len = u32::try_from(value.len()).map_err(|_| "index string is too long".to_string())?;
    writer
        .write_all(&len.to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(value.as_bytes())
        .map_err(|e| e.to_string())
}

pub(crate) fn read_u32_bytes(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    let end = cursor.saturating_add(4);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| "truncated index sidecar".to_string())?;
    *cursor = end;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn read_u64_bytes(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let end = cursor.saturating_add(8);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| "truncated index sidecar".to_string())?;
    *cursor = end;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn read_i64_bytes(bytes: &[u8], cursor: &mut usize) -> Result<i64, String> {
    Ok(read_u64_bytes(bytes, cursor)? as i64)
}

pub(crate) fn decode_optional_i64(value: i64) -> Option<i64> {
    (value != i64::MIN).then_some(value)
}

pub(crate) fn encode_optional_i64(value: Option<i64>) -> i64 {
    value.unwrap_or(i64::MIN)
}

pub(crate) fn read_string_bytes<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
) -> Result<&'a str, String> {
    let len = read_u32_bytes(bytes, cursor)? as usize;
    let end = cursor.saturating_add(len);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| "truncated index sidecar string".to_string())?;
    *cursor = end;
    std::str::from_utf8(value).map_err(|e| e.to_string())
}

pub(crate) fn open_index_manifest(root_key: &str) -> Result<Option<Mmap>, String> {
    let Some(path) = index_manifest_path(root_key) else {
        return Ok(None);
    };
    let Ok(file) = File::open(path) else {
        return Ok(None);
    };
    // The manifest is replaced atomically and remains immutable while mapped.
    let mmap = unsafe { Mmap::map(&file) }.map_err(|e| e.to_string())?;
    Ok(Some(mmap))
}

pub(crate) fn parse_index_manifest<'a>(
    mmap: &'a Mmap,
    root_key: &str,
    fingerprint: &str,
) -> Result<Vec<ExistingIndexEntry<'a>>, String> {
    let bytes = mmap.as_ref();
    if bytes.get(..8) != Some(INDEX_MANIFEST_MAGIC) {
        return Err("unsupported index manifest version".to_string());
    }
    let mut cursor = 8;
    if read_string_bytes(bytes, &mut cursor)? != root_key {
        return Err("index manifest root mismatch".to_string());
    }
    if read_string_bytes(bytes, &mut cursor)? != fingerprint {
        return Err("index manifest fingerprint mismatch".to_string());
    }
    let count = usize::try_from(read_u64_bytes(bytes, &mut cursor)?)
        .map_err(|_| "index manifest is too large".to_string())?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let dir_id = read_i64_bytes(bytes, &mut cursor)?;
        let name_id = read_i64_bytes(bytes, &mut cursor)?;
        let kind = *bytes
            .get(cursor)
            .ok_or_else(|| "truncated index manifest kind".to_string())? as i64;
        cursor += 1;
        let mtime = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let size = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let allocated_size = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let activity = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let device = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let inode = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let link_count = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let path = read_string_bytes(bytes, &mut cursor)?;
        entries.push(ExistingIndexEntry {
            dir_id,
            name_id,
            path: Cow::Borrowed(path),
            kind,
            mtime,
            size,
            allocated_size,
            activity,
            device,
            inode,
            link_count,
        });
    }
    if cursor != bytes.len() {
        return Err("index manifest has trailing data".to_string());
    }
    Ok(entries)
}

pub(crate) fn begin_index_manifest(
    root_key: &str,
    fingerprint: &str,
    count: usize,
) -> Result<(PathBuf, PathBuf, BufWriter<File>), String> {
    let path = index_manifest_path(root_key)
        .ok_or_else(|| "unable to resolve index manifest path".to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| "invalid manifest path".to_string())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("manifest.tmp.{}", unique_temp_tag()));
    let mut writer =
        BufWriter::with_capacity(1024 * 1024, File::create(&tmp).map_err(|e| e.to_string())?);
    writer
        .write_all(INDEX_MANIFEST_MAGIC)
        .map_err(|e| e.to_string())?;
    write_len_prefixed(&mut writer, root_key)?;
    write_len_prefixed(&mut writer, fingerprint)?;
    writer
        .write_all(&(count as u64).to_le_bytes())
        .map_err(|e| e.to_string())?;
    Ok((path, tmp, writer))
}

pub(crate) fn write_index_manifest_record(
    writer: &mut impl Write,
    entry: &PendingIndexEntry,
    path: &str,
) -> Result<(), String> {
    writer
        .write_all(&entry.dir_id.to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&entry.name_id.to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&[entry.kind as u8])
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.mtime).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.size).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.allocated_size).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.activity).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.device).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.inode).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.link_count).to_le_bytes())
        .map_err(|e| e.to_string())?;
    write_len_prefixed(writer, path)
}

pub(crate) fn finish_index_manifest(
    path: PathBuf,
    tmp: PathBuf,
    mut writer: BufWriter<File>,
) -> Result<(), String> {
    writer.flush().map_err(|e| e.to_string())?;
    writer.get_ref().sync_all().map_err(|e| e.to_string())?;
    drop(writer);
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    sync_parent_dir(&path)
}

pub(crate) fn write_index_snapshot_from_db(
    conn: &Connection,
    root_key: &str,
) -> Result<(), String> {
    let snapshot_path = index_snapshot_path(root_key)
        .ok_or_else(|| "unable to resolve the fsx cache directory".to_string())?;
    let parent = snapshot_path
        .parent()
        .ok_or_else(|| "invalid index snapshot path".to_string())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp_path = snapshot_path.with_extension(format!("snapshot.tmp.{}", unique_temp_tag()));

    let result = (|| -> Result<(), String> {
        let file = File::create(&tmp_path).map_err(|e| e.to_string())?;
        let mut writer = BufWriter::with_capacity(1024 * 1024, file);
        writer
            .write_all(INDEX_SNAPSHOT_MAGIC)
            .map_err(|e| e.to_string())?;
        let root_len = u32::try_from(root_key.len())
            .map_err(|_| "indexed root path is too long".to_string())?;
        writer
            .write_all(&root_len.to_le_bytes())
            .map_err(|e| e.to_string())?;
        writer
            .write_all(root_key.as_bytes())
            .map_err(|e| e.to_string())?;
        let counts_offset = writer.stream_position().map_err(|e| e.to_string())?;
        writer.write_all(&[0u8; 24]).map_err(|e| e.to_string())?;

        let root_prefix = index_path_prefix(root_key);
        let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
        let mut stmt = conn
            .prepare(
                "SELECT d.path, s.value, e.kind
                 FROM dirs d
                 CROSS JOIN entries e INDEXED BY idx_entries_dir ON e.dir_id = d.id
                 JOIN strings s ON e.name_id = s.id
                 WHERE d.path = ?1 OR (d.path >= ?2 AND d.path < ?3)",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(params![root_key, root_prefix, root_prefix_end])
            .map_err(|e| e.to_string())?;
        let mut all_count = 0u64;
        let mut file_count = 0u64;
        let mut dir_count = 0u64;

        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let dir_path: String = row.get(0).map_err(|e| e.to_string())?;
            let name: String = row.get(1).map_err(|e| e.to_string())?;
            let kind: u8 = row.get(2).map_err(|e| e.to_string())?;
            let mut path = if dir_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", dir_path, name)
            };
            if kind == 1 {
                path.push('/');
                dir_count = dir_count.saturating_add(1);
            } else if kind == 0 {
                file_count = file_count.saturating_add(1);
            }
            all_count = all_count.saturating_add(1);

            let path_len = u32::try_from(path.len())
                .map_err(|_| format!("indexed path is too long: {}", path))?;
            writer.write_all(&[kind]).map_err(|e| e.to_string())?;
            writer
                .write_all(&path_len.to_le_bytes())
                .map_err(|e| e.to_string())?;
            writer
                .write_all(path.as_bytes())
                .map_err(|e| e.to_string())?;
        }

        writer.flush().map_err(|e| e.to_string())?;
        writer
            .seek(SeekFrom::Start(counts_offset))
            .map_err(|e| e.to_string())?;
        for count in [all_count, file_count, dir_count] {
            writer
                .write_all(&count.to_le_bytes())
                .map_err(|e| e.to_string())?;
        }
        writer.flush().map_err(|e| e.to_string())?;
        writer.get_ref().sync_all().map_err(|e| e.to_string())?;
        drop(writer);
        fs::rename(&tmp_path, &snapshot_path).map_err(|e| e.to_string())?;
        fs::set_permissions(&snapshot_path, fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        sync_parent_dir(&snapshot_path)?;
        if let Some(legacy_path) = index_state_path(root_key, "snapshot") {
            if legacy_path != snapshot_path {
                let _ = fs::remove_file(legacy_path);
            }
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

pub(crate) fn rebuild_index_snapshot(root_raw: &str) -> Result<(), String> {
    rebuild_index_snapshot_path(Path::new(&expand_home_path(root_raw)))
}

pub(crate) fn rebuild_index_snapshot_path(root_raw: &Path) -> Result<(), String> {
    let root_key = normalize_index_root_path(root_raw)?;
    let conn = open_index_db_for_search()?;
    if !index_root_is_known(&conn, &root_key)? {
        return Err(format!(
            "'{}' is not present in the unearth index",
            root_key
        ));
    }
    let fingerprint_key = index_fingerprint_key(&root_key);
    let fingerprint = index_fingerprint_value(&conn, &fingerprint_key)?
        .ok_or_else(|| format!("'{}' has no committed fingerprint", root_key))?;
    rebuild_index_base_sidecars(&conn, &root_key, &fingerprint)
}

pub(crate) fn path_age_at_least(path: &Path, min_age: Duration) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    modified.elapsed().map(|age| age >= min_age).unwrap_or(true)
}

pub(crate) fn write_stamp(path: &Path) {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let _ = fs::create_dir_all(parent);
    }
    if File::create(path).is_ok() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

pub(crate) fn initialize_index_db() -> Result<Connection, String> {
    let path = index_db_path().ok_or_else(|| "Could not determine fsx cache dir".to_string())?;
    let custom_database = std::env::var_os("FSX_INDEX_DB").is_some();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let parent_existed = fs::symlink_metadata(parent).is_ok();
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("index database parent must be a real directory".to_string());
        }
        // The default cache owns its directory hierarchy. A custom database
        // may live in a project/shared directory; never chmod an arbitrary
        // existing ancestor just because FSX_INDEX_DB points there.
        if !custom_database || !parent_existed {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        if !custom_database {
            if let Some(cache_root) = parent.parent() {
                fs::set_permissions(cache_root, fs::Permissions::from_mode(0o700))
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    migrate_legacy_index(&path)?;
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(30))
        .map_err(|e| e.to_string())?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -8192;
        CREATE TABLE IF NOT EXISTS strings (
            id INTEGER PRIMARY KEY,
            value TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS dirs (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS entries (
            id INTEGER PRIMARY KEY,
            dir_id INTEGER NOT NULL,
            name_id INTEGER NOT NULL,
            kind INTEGER NOT NULL,
            mtime INTEGER,
            size INTEGER,
            allocated_size INTEGER,
            activity INTEGER,
            UNIQUE(dir_id, name_id, kind)
        );
        CREATE TABLE IF NOT EXISTS dir_stats (
            dir_id INTEGER PRIMARY KEY,
            allocated_size INTEGER NOT NULL DEFAULT 0,
            files INTEGER NOT NULL DEFAULT 0,
            dirs INTEGER NOT NULL DEFAULT 0,
            missing_sizes INTEGER NOT NULL DEFAULT 0,
            missing_hardlink_metadata INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS indexed_roots (
            root TEXT PRIMARY KEY,
            refreshed_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS index_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS actors (
            id INTEGER PRIMARY KEY,
            executable TEXT NOT NULL UNIQUE,
            classification TEXT NOT NULL,
            first_seen INTEGER NOT NULL,
            last_seen INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS watch_state (
            root TEXT PRIMARY KEY,
            backend TEXT NOT NULL,
            status TEXT NOT NULL,
            generation INTEGER NOT NULL DEFAULT 0,
            last_event INTEGER,
            last_reconcile INTEGER,
            dirty INTEGER NOT NULL DEFAULT 0,
            online INTEGER NOT NULL DEFAULT 1,
            watcher_pid INTEGER,
            error TEXT,
            owner_boot_id TEXT,
            owner_starttime INTEGER,
            heartbeat INTEGER
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS strings_fts USING fts5(
            value,
            content='strings',
            content_rowid='id',
            tokenize='trigram'
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS dirs_fts USING fts5(
            path,
            content='dirs',
            content_rowid='id',
            tokenize='trigram'
        );
        CREATE TRIGGER IF NOT EXISTS strings_fts_ai AFTER INSERT ON strings BEGIN
            INSERT INTO strings_fts(rowid, value) VALUES (new.id, new.value);
        END;
        CREATE TRIGGER IF NOT EXISTS strings_fts_ad AFTER DELETE ON strings BEGIN
            INSERT INTO strings_fts(strings_fts, rowid, value)
            VALUES ('delete', old.id, old.value);
        END;
        CREATE TRIGGER IF NOT EXISTS strings_fts_au AFTER UPDATE ON strings BEGIN
            INSERT INTO strings_fts(strings_fts, rowid, value)
            VALUES ('delete', old.id, old.value);
            INSERT INTO strings_fts(rowid, value) VALUES (new.id, new.value);
        END;
        CREATE TRIGGER IF NOT EXISTS dirs_fts_ai AFTER INSERT ON dirs BEGIN
            INSERT INTO dirs_fts(rowid, path) VALUES (new.id, new.path);
        END;
        CREATE TRIGGER IF NOT EXISTS dirs_fts_ad AFTER DELETE ON dirs BEGIN
            INSERT INTO dirs_fts(dirs_fts, rowid, path)
            VALUES ('delete', old.id, old.path);
        END;
        CREATE TRIGGER IF NOT EXISTS dirs_fts_au AFTER UPDATE ON dirs BEGIN
            INSERT INTO dirs_fts(dirs_fts, rowid, path)
            VALUES ('delete', old.id, old.path);
            INSERT INTO dirs_fts(rowid, path) VALUES (new.id, new.path);
        END;
        CREATE INDEX IF NOT EXISTS idx_entries_dir ON entries(dir_id);
        CREATE INDEX IF NOT EXISTS idx_entries_name ON entries(name_id);
        ",
    )
    .map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("PRAGMA table_info(entries)")
        .map_err(|e| e.to_string())?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    for column in [
        "activity INTEGER",
        "allocated_size INTEGER",
        "device INTEGER",
        "inode INTEGER",
        "link_count INTEGER",
    ] {
        let name = column.split_whitespace().next().unwrap_or_default();
        if !columns.iter().any(|existing| existing == name) {
            conn.execute(&format!("ALTER TABLE entries ADD COLUMN {column}"), [])
                .map_err(|e| e.to_string())?;
        }
    }
    for column in [
        "event_kind INTEGER",
        "actor_id INTEGER",
        "actor_uid INTEGER",
        "actor_pid INTEGER",
        "event_at INTEGER",
    ] {
        let name = column.split_whitespace().next().unwrap_or_default();
        if !columns.iter().any(|existing| existing == name) {
            conn.execute(&format!("ALTER TABLE entries ADD COLUMN {column}"), [])
                .map_err(|e| e.to_string())?;
        }
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_entries_kind_activity ON entries(kind, activity DESC)",
        [],
    )
    .map_err(|e| e.to_string())?;
    conn.execute_batch(
        "
        DROP INDEX IF EXISTS idx_entries_hardlink_dir;
        CREATE INDEX IF NOT EXISTS idx_entries_hardlink_candidates
            ON entries(dir_id, device, inode, allocated_size)
            WHERE link_count > 1 AND kind <> 1;
        CREATE TRIGGER IF NOT EXISTS entries_stats_ai AFTER INSERT ON entries BEGIN
            INSERT INTO dir_stats(
                dir_id, allocated_size, files, dirs, missing_sizes,
                missing_hardlink_metadata
            ) VALUES (
                new.dir_id,
                COALESCE(new.allocated_size, 0),
                CASE WHEN new.kind <> 1 THEN 1 ELSE 0 END,
                CASE WHEN new.kind = 1 THEN 1 ELSE 0 END,
                CASE WHEN new.allocated_size IS NULL THEN 1 ELSE 0 END,
                CASE WHEN new.link_count IS NULL THEN 1 ELSE 0 END
            ) ON CONFLICT(dir_id) DO UPDATE SET
                allocated_size = dir_stats.allocated_size + excluded.allocated_size,
                files = dir_stats.files + excluded.files,
                dirs = dir_stats.dirs + excluded.dirs,
                missing_sizes = dir_stats.missing_sizes + excluded.missing_sizes,
                missing_hardlink_metadata = dir_stats.missing_hardlink_metadata
                    + excluded.missing_hardlink_metadata;
        END;
        CREATE TRIGGER IF NOT EXISTS entries_stats_ad AFTER DELETE ON entries BEGIN
            UPDATE dir_stats SET
                allocated_size = allocated_size - COALESCE(old.allocated_size, 0),
                files = files - CASE WHEN old.kind <> 1 THEN 1 ELSE 0 END,
                dirs = dirs - CASE WHEN old.kind = 1 THEN 1 ELSE 0 END,
                missing_sizes = missing_sizes
                    - CASE WHEN old.allocated_size IS NULL THEN 1 ELSE 0 END,
                missing_hardlink_metadata = missing_hardlink_metadata
                    - CASE WHEN old.link_count IS NULL THEN 1 ELSE 0 END
            WHERE dir_id = old.dir_id;
        END;
        CREATE TRIGGER IF NOT EXISTS entries_stats_au
        AFTER UPDATE OF dir_id, kind, allocated_size, link_count ON entries BEGIN
            UPDATE dir_stats SET
                allocated_size = allocated_size - COALESCE(old.allocated_size, 0),
                files = files - CASE WHEN old.kind <> 1 THEN 1 ELSE 0 END,
                dirs = dirs - CASE WHEN old.kind = 1 THEN 1 ELSE 0 END,
                missing_sizes = missing_sizes
                    - CASE WHEN old.allocated_size IS NULL THEN 1 ELSE 0 END,
                missing_hardlink_metadata = missing_hardlink_metadata
                    - CASE WHEN old.link_count IS NULL THEN 1 ELSE 0 END
            WHERE dir_id = old.dir_id;
            INSERT INTO dir_stats(
                dir_id, allocated_size, files, dirs, missing_sizes,
                missing_hardlink_metadata
            ) VALUES (
                new.dir_id,
                COALESCE(new.allocated_size, 0),
                CASE WHEN new.kind <> 1 THEN 1 ELSE 0 END,
                CASE WHEN new.kind = 1 THEN 1 ELSE 0 END,
                CASE WHEN new.allocated_size IS NULL THEN 1 ELSE 0 END,
                CASE WHEN new.link_count IS NULL THEN 1 ELSE 0 END
            ) ON CONFLICT(dir_id) DO UPDATE SET
                allocated_size = dir_stats.allocated_size + excluded.allocated_size,
                files = dir_stats.files + excluded.files,
                dirs = dir_stats.dirs + excluded.dirs,
                missing_sizes = dir_stats.missing_sizes + excluded.missing_sizes,
                missing_hardlink_metadata = dir_stats.missing_hardlink_metadata
                    + excluded.missing_hardlink_metadata;
        END;
        CREATE TRIGGER IF NOT EXISTS dirs_stats_ad AFTER DELETE ON dirs BEGIN
            DELETE FROM dir_stats WHERE dir_id = old.id;
        END;
        ",
    )
    .map_err(|e| e.to_string())?;
    let direct_stats_version = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'dir_stats_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();
    if direct_stats_version != "1" {
        conn.execute_batch(
            "
            BEGIN IMMEDIATE;
            DELETE FROM dir_stats;
            INSERT INTO dir_stats(
                dir_id, allocated_size, files, dirs, missing_sizes,
                missing_hardlink_metadata
            )
            SELECT
                dir_id,
                COALESCE(SUM(COALESCE(allocated_size, 0)), 0),
                COALESCE(SUM(CASE WHEN kind <> 1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN kind = 1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN allocated_size IS NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN link_count IS NULL THEN 1 ELSE 0 END), 0)
            FROM entries
            GROUP BY dir_id;
            INSERT OR REPLACE INTO index_meta(key, value)
            VALUES ('dir_stats_version', '1');
            COMMIT;
            ",
        )
        .map_err(|e| e.to_string())?;
    }
    let mut stmt = conn
        .prepare("PRAGMA table_info(watch_state)")
        .map_err(|e| e.to_string())?;
    let watch_columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    for column in [
        "owner_boot_id TEXT",
        "owner_starttime INTEGER",
        "heartbeat INTEGER",
    ] {
        let name = column.split_whitespace().next().unwrap_or_default();
        if !watch_columns.iter().any(|existing| existing == name) {
            conn.execute(&format!("ALTER TABLE watch_state ADD COLUMN {column}"), [])
                .map_err(|e| e.to_string())?;
        }
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        if sidecar.is_file() {
            fs::set_permissions(sidecar, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
    }
    conn.execute_batch(
        "
        DROP INDEX IF EXISTS idx_strings_value;
        DROP INDEX IF EXISTS idx_dirs_path;
        CREATE INDEX IF NOT EXISTS idx_entries_dir_kind_activity
            ON entries(dir_id, kind, activity DESC);
        PRAGMA user_version = 4;
        ",
    )
    .map_err(|e| e.to_string())?;
    ensure_index_search_ready(&conn)?;
    Ok(conn)
}

pub(crate) fn open_index_db_writer() -> Result<Connection, String> {
    initialize_index_db()
}

pub(crate) fn open_index_db_readonly() -> Result<Connection, String> {
    let path = fsx::index::database_read_path()
        .ok_or_else(|| "Could not determine fsx cache dir".to_string())?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(30))
        .map_err(|e| e.to_string())?;
    conn.execute_batch(
        "PRAGMA query_only = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -8192;
         PRAGMA mmap_size = 268435456;",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn migrate_legacy_index(path: &Path) -> Result<(), String> {
    if path.is_file() {
        return Ok(());
    }
    let Some(legacy) = legacy_index_db_path().filter(|candidate| candidate.is_file()) else {
        return Ok(());
    };
    let parent = path
        .parent()
        .ok_or_else(|| "canonical index has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_file_name(format!(
        ".{}.migration-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("fsx"),
        std::process::id()
    ));
    let _ = fs::remove_file(&temporary);
    let legacy_conn = Connection::open_with_flags(
        &legacy,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| format!("could not open legacy index {}: {error}", legacy.display()))?;
    legacy_conn
        .execute("VACUUM INTO ?1", [&temporary.to_string_lossy().to_string()])
        .map_err(|error| format!("could not snapshot legacy index: {error}"))?;
    drop(legacy_conn);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!(
            "could not publish migrated index {}: {error}",
            path.display()
        )
    })?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("could not sync migrated index directory: {error}"))?;
    Ok(())
}

pub(crate) fn open_index_db_for_search() -> Result<Connection, String> {
    let needs_migration = match open_index_db_readonly() {
        Ok(conn) => conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .map(|version| version < INDEX_SCHEMA_VERSION)
            .unwrap_or(true),
        Err(_) => true,
    };
    if needs_migration {
        let _ = initialize_index_db()?;
    }
    open_index_db_readonly()
}

pub(crate) fn index_search_is_ready(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM index_meta WHERE key = 'fts_trigram_v1'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}
