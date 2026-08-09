#[cfg(feature = "watcher")]
use super::super::cli::parse_args_from;
use super::*;
#[cfg(feature = "watcher")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "watcher")]
use std::sync::Arc;
#[cfg(feature = "watcher")]
pub(crate) struct QueryServerHandle {
    stop: Arc<AtomicBool>,
    socket: PathBuf,
    join: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "watcher")]
impl Drop for QueryServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

pub(crate) fn write_query_string(stream: &mut UnixStream, value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let length = u32::try_from(bytes.len()).map_err(|_| "query string is too long".to_string())?;
    stream
        .write_all(&length.to_le_bytes())
        .and_then(|_| stream.write_all(bytes))
        .map_err(|e| e.to_string())
}

pub(crate) fn read_query_string(reader: &mut BufReader<UnixStream>) -> Result<String, String> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length).map_err(|e| e.to_string())?;
    let length = u32::from_le_bytes(length) as usize;
    if length > QUERY_MAX_FRAME {
        return Err("query string exceeds protocol limit".to_string());
    }
    let mut bytes = vec![0u8; length];
    reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

pub(crate) fn write_query_request(
    stream: &mut UnixStream,
    opts: &Options,
    root: &Path,
    terms: &[String],
) -> Result<(), String> {
    let mut flags = 0u8;
    if opts.force_full {
        flags |= 1;
    }
    if opts.regex_mode {
        flags |= 1 << 1;
    }
    if opts.force_dir {
        flags |= 1 << 2;
    }
    if opts.force_file {
        flags |= 1 << 3;
    }
    if !opts.visible_only {
        flags |= 1 << 4;
    }
    if opts.recent_limit.is_some() {
        flags |= 1 << 5;
    }
    if opts.limit.is_some() {
        flags |= 1 << 6;
    }
    stream
        .write_all(QUERY_PROTOCOL_MAGIC)
        .and_then(|_| stream.write_all(&[flags]))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(
            &u64::try_from(opts.recent_limit.unwrap_or(0))
                .map_err(|_| "recent limit is too large".to_string())?
                .to_le_bytes(),
        )
        .and_then(|_| {
            let field = match opts.sort_field {
                Some(SortField::Date) => 1,
                Some(SortField::Size) => 2,
                Some(SortField::Name) => 3,
                None => 0,
            };
            let order = match opts.sort_order {
                Some(SortOrder::Asc) => 1,
                Some(SortOrder::Desc) | None => 2,
            };
            stream.write_all(&[field << 2 | order])
        })
        .and_then(|_| {
            stream.write_all(
                &u64::try_from(opts.limit.unwrap_or(0))
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "limit too large"))?
                    .to_le_bytes(),
            )
        })
        .map_err(|e| e.to_string())?;
    write_query_string(stream, &normalize_index_dir(root))?;
    let term_count = u32::try_from(terms.len()).map_err(|_| "too many query terms".to_string())?;
    stream
        .write_all(&term_count.to_le_bytes())
        .map_err(|e| e.to_string())?;
    for term in terms {
        write_query_string(stream, term)?;
    }
    Ok(())
}

#[cfg(feature = "watcher")]
pub(crate) fn read_query_request(reader: &mut BufReader<UnixStream>) -> Result<Options, String> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic).map_err(|e| e.to_string())?;
    if &magic != QUERY_PROTOCOL_MAGIC {
        return Err("invalid unearth query protocol header".to_string());
    }
    let mut flags = [0u8; 1];
    reader.read_exact(&mut flags).map_err(|e| e.to_string())?;
    if flags[0] & !0x7f != 0 {
        return Err("invalid unearth query flags".to_string());
    }
    let mut recent_limit = [0u8; 8];
    reader
        .read_exact(&mut recent_limit)
        .map_err(|e| e.to_string())?;
    let recent_limit = u64::from_le_bytes(recent_limit);
    let mut sort_code = [0u8; 1];
    reader
        .read_exact(&mut sort_code)
        .map_err(|e| e.to_string())?;
    let mut limit_bytes = [0u8; 8];
    reader
        .read_exact(&mut limit_bytes)
        .map_err(|e| e.to_string())?;
    let limit = u64::from_le_bytes(limit_bytes);
    let sort_field = match sort_code[0] >> 2 {
        0 => None,
        1 => Some("date"),
        2 => Some("size"),
        3 => Some("name"),
        _ => return Err("invalid query sort field".to_string()),
    };
    let sort_order = match sort_code[0] & 3 {
        1 => "asc",
        2 => "desc",
        _ => return Err("invalid query sort order".to_string()),
    };
    let root = read_query_string(reader)?;
    let mut request_bytes = root.len();
    if request_bytes > QUERY_MAX_REQUEST {
        return Err("query request exceeds protocol limit".to_string());
    }
    let mut count = [0u8; 4];
    reader.read_exact(&mut count).map_err(|e| e.to_string())?;
    let count = u32::from_le_bytes(count) as usize;
    if count > 256 {
        return Err("query has too many terms".to_string());
    }
    let mut terms = Vec::with_capacity(count);
    for _ in 0..count {
        let term = read_query_string(reader)?;
        request_bytes = request_bytes
            .checked_add(term.len())
            .ok_or_else(|| "query request exceeds protocol limit".to_string())?;
        if request_bytes > QUERY_MAX_REQUEST {
            return Err("query request exceeds protocol limit".to_string());
        }
        terms.push(term);
    }

    let mut args = vec!["--index".to_string()];
    if flags[0] & (1 << 5) != 0 {
        let limit =
            usize::try_from(recent_limit).map_err(|_| "recent limit is too large".to_string())?;
        if limit == 0 {
            return Err("recent query has no limit".to_string());
        }
        args.push("--recent".to_string());
        args.push(limit.to_string());
        args.extend([
            "--sort".to_string(),
            "date".to_string(),
            sort_order.to_string(),
        ]);
    } else if let Some(field) = sort_field {
        args.extend([
            "--sort".to_string(),
            field.to_string(),
            sort_order.to_string(),
        ]);
    }
    if flags[0] & 1 != 0 {
        args.push("--full".to_string());
    }
    if flags[0] & (1 << 1) != 0 {
        args.push("--regex".to_string());
    }
    if flags[0] & (1 << 2) != 0 {
        args.push("--dir".to_string());
    }
    if flags[0] & (1 << 3) != 0 {
        args.push("--file".to_string());
    }
    if flags[0] & (1 << 4) != 0 {
        args.push("--hidden".to_string());
    }
    args.extend(terms);
    args.push(root);
    if flags[0] & (1 << 6) != 0 {
        let limit = usize::try_from(limit).map_err(|_| "limit is too large".to_string())?;
        if limit == 0 {
            return Err("query limit must be positive".to_string());
        }
        args.extend(["--limit".to_string(), limit.to_string()]);
    }
    parse_args_from(args)
}

#[cfg(feature = "watcher")]
pub(crate) fn write_query_record(
    stream: &mut UnixStream,
    result: &SearchResult,
) -> Result<(), String> {
    let path = result.path.as_bytes();
    let length = path
        .len()
        .checked_add(17)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| "query result path is too long".to_string())?;
    stream
        .write_all(&length.to_le_bytes())
        .and_then(|_| {
            stream.write_all(&[u8::from(result.is_dir) | (u8::from(result.is_symlink) << 1)])
        })
        .and_then(|_| {
            stream.write_all(
                &result
                    .indexed_activity_nanos
                    .unwrap_or(i64::MIN)
                    .to_le_bytes(),
            )
        })
        .and_then(|_| {
            stream.write_all(
                &result
                    .indexed_size
                    .and_then(|size| i64::try_from(size).ok())
                    .unwrap_or(i64::MIN)
                    .to_le_bytes(),
            )
        })
        .and_then(|_| stream.write_all(path))
        .map_err(|e| e.to_string())
}

#[cfg(feature = "watcher")]
pub(crate) fn write_query_error(stream: &mut UnixStream, error: &str) -> Result<(), String> {
    stream
        .write_all(QUERY_RESPONSE_MAGIC)
        .map_err(|e| e.to_string())?;
    stream.write_all(&[1]).map_err(|e| e.to_string())?;
    write_query_string(stream, error)
}

#[cfg(feature = "watcher")]
pub(crate) fn handle_query_connection(
    mut stream: UnixStream,
    conn: &Connection,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let opts = match read_query_request(&mut reader) {
        Ok(opts) => opts,
        Err(error) => {
            let _ = write_query_error(&mut stream, &error);
            return Ok(());
        }
    };
    stream
        .write_all(QUERY_RESPONSE_MAGIC)
        .and_then(|_| stream.write_all(&[0]))
        .map_err(|e| e.to_string())?;
    let result = if opts.recent_limit.is_some() {
        query_recent_rows(&opts, conn, |result| {
            write_query_record(&mut stream, &result)
        })
    } else {
        query_indexed_rows(&opts, None, conn, |result| {
            write_query_record(&mut stream, &result)
        })
    };
    result?;
    stream
        .write_all(&0u32.to_le_bytes())
        .map_err(|e| e.to_string())
}

#[cfg(feature = "watcher")]
pub(crate) fn query_server_loop(listener: UnixListener, stop: Arc<AtomicBool>) {
    let (sender, receiver) = crossbeam_channel::bounded::<UnixStream>(64);
    let workers = (0..4)
        .filter_map(|index| {
            let receiver = receiver.clone();
            let worker_stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name(format!("unearthd-query-{index}"))
                .spawn(move || {
                    let Ok(conn) = open_index_db_readonly() else {
                        return;
                    };
                    while !worker_stop.load(Ordering::Acquire) {
                        match receiver.recv_timeout(Duration::from_millis(100)) {
                            Ok(stream) => {
                                if worker_stop.load(Ordering::Acquire) {
                                    break;
                                }
                                let _ = handle_query_connection(stream, &conn);
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .ok()
        })
        .collect::<Vec<_>>();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if sender.send(stream).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    drop(sender);
    for worker in workers {
        let _ = worker.join();
    }
}

#[cfg(feature = "watcher")]
pub(crate) fn start_query_server() -> Result<QueryServerHandle, String> {
    let socket =
        query_socket_path().ok_or_else(|| "Could not determine unearth cache dir".to_string())?;
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&socket).is_ok() {
                return Err("another unearthd query server is already running".to_string());
            }
            fs::remove_file(&socket).map_err(|e| e.to_string())?;
            UnixListener::bind(&socket).map_err(|e| e.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("unearthd-query".to_string())
        .spawn(move || query_server_loop(listener, thread_stop))
        .map_err(|e| e.to_string())?;
    Ok(QueryServerHandle {
        stop,
        socket,
        join: Some(join),
    })
}

pub(crate) fn ensure_index_search_ready(conn: &Connection) -> Result<bool, String> {
    let ready: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM index_meta WHERE key = 'fts_trigram_v1'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if ready > 0 {
        return Ok(true);
    }
    conn.execute_batch(
        "
        INSERT INTO strings_fts(strings_fts) VALUES ('rebuild');
        INSERT INTO dirs_fts(dirs_fts) VALUES ('rebuild');
        INSERT OR REPLACE INTO index_meta(key, value)
        VALUES ('fts_trigram_v1', 'ready');
        ",
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
}
