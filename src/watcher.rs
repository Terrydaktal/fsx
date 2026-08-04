use super::*;
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use jwalk::{Parallelism, WalkDir};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const EVENT_CREATE: i64 = 1;
const EVENT_MODIFY: i64 = 2;
const EVENT_DELETE: i64 = 3;
const EVENT_MOVE: i64 = 4;
const EVENT_ATTRIB: i64 = 5;
const EVENT_RECONCILE: i64 = 6;
const EVENT_OVERFLOW: i64 = 7;

const ACTOR_UNKNOWN: &str = "unknown";
const ACTOR_RECONCILE: &str = "unearth:reconcile";
const WATCH_BATCH_MAX: usize = 4096;
const WATCH_CHANNEL_CAPACITY: usize = 16_384;
const WATCH_BATCH_DELAY: Duration = Duration::from_millis(125);
const WATCH_POLL_DELAY: Duration = Duration::from_millis(40);
const PENDING_MOVE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(unix)]
static WATCH_STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_watch_signal(_signal: libc::c_int) {
    WATCH_STOP.store(true, Ordering::Relaxed);
}

#[cfg(unix)]
fn install_watch_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, handle_watch_signal as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_watch_signal as libc::sighandler_t);
    }
}

#[derive(Clone, Debug)]
struct Actor {
    executable: String,
    classification: String,
    uid: Option<i64>,
    pid: Option<i64>,
}

impl Actor {
    fn unknown() -> Self {
        Self {
            executable: ACTOR_UNKNOWN.to_string(),
            classification: ACTOR_UNKNOWN.to_string(),
            uid: None,
            pid: None,
        }
    }

    fn reconcile() -> Self {
        Self {
            executable: ACTOR_RECONCILE.to_string(),
            classification: "reconcile".to_string(),
            uid: None,
            pid: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Upsert,
    Remove,
    Move,
    Reconcile,
    Overflow,
}

#[derive(Clone, Debug)]
struct FsEvent {
    action: Action,
    path: PathBuf,
    old_path: Option<PathBuf>,
    is_dir: bool,
    actor: Actor,
    event_kind: i64,
    at: i64,
}

#[derive(Clone, Debug)]
struct RootState {
    key: String,
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct WatcherOwner {
    pid: i64,
    boot_id: String,
    starttime: i64,
}

type ExistingOwner = (Option<i64>, Option<String>, Option<i64>, Option<String>);

fn current_owner() -> WatcherOwner {
    let pid = i64::from(std::process::id());
    WatcherOwner {
        pid,
        boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|value| value.trim().to_string())
            .unwrap_or_default(),
        starttime: process_starttime(pid).unwrap_or_default(),
    }
}

fn process_starttime(pid: i64) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(") ")?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn owner_is_alive(pid: Option<i64>, boot_id: Option<&str>, starttime: Option<i64>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    if let (Some(expected_boot), Some(expected_start)) = (boot_id, starttime) {
        if expected_boot.is_empty() {
            return false;
        }
        let current_boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        return current_boot == expected_boot
            && process_starttime(pid).is_some_and(|actual| actual == expected_start);
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

fn claim_watch_states(roots: &[RootState]) -> Result<(), String> {
    let owner = current_owner();
    let conn = open_index_db()?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for root in roots {
        let existing: Option<ExistingOwner> = tx
            .query_row(
                "SELECT watcher_pid, owner_boot_id, owner_starttime, status
                 FROM watch_state WHERE root=?1",
                [&root.key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((pid, boot_id, starttime, status)) = existing {
            let same_owner = pid == Some(owner.pid)
                && boot_id.as_deref() == Some(owner.boot_id.as_str())
                && starttime == Some(owner.starttime);
            if !same_owner
                && matches!(
                    status.as_deref(),
                    Some("starting" | "running" | "recovering")
                )
                && owner_is_alive(pid, boot_id.as_deref(), starttime)
            {
                return Err(format!(
                    "a live watcher already owns {} (pid {})",
                    root.key,
                    pid.unwrap_or_default()
                ));
            }
        }
        tx.execute(
            "INSERT INTO watch_state(root, backend, status, generation, dirty, online,
                                     watcher_pid, error, owner_boot_id, owner_starttime, heartbeat)
             VALUES (?1, 'starting', 'starting', 0, 1, 1, ?2, NULL, ?3, ?4, ?5)
             ON CONFLICT(root) DO UPDATE SET backend='starting', status='starting', dirty=1,
                 online=1, watcher_pid=excluded.watcher_pid, error=NULL,
                 owner_boot_id=excluded.owner_boot_id, owner_starttime=excluded.owner_starttime,
                 heartbeat=excluded.heartbeat",
            params![
                root.key,
                owner.pid,
                owner.boot_id,
                owner.starttime,
                now_seconds()
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

fn heartbeat_states(roots: &[RootState], backend: &str) -> Result<(), String> {
    let owner = current_owner();
    let conn = open_index_db()?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for root in roots {
        tx.execute(
            "UPDATE watch_state SET backend=?2, heartbeat=?3, watcher_pid=?4
             WHERE root=?1 AND owner_boot_id=?5 AND owner_starttime=?6",
            params![
                root.key,
                backend,
                now_seconds(),
                owner.pid,
                owner.boot_id,
                owner.starttime
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

fn shutdown_states(roots: &[RootState]) {
    let owner = current_owner();
    if let Ok(conn) = open_index_db() {
        for root in roots {
            let _ = conn.execute(
                "UPDATE watch_state SET status='stopped', online=0, dirty=0,
                 error=NULL WHERE root=?1 AND owner_boot_id=?2 AND owner_starttime=?3",
                params![root.key, owner.boot_id, owner.starttime],
            );
        }
    }
}

fn periodic_reconcile_interval() -> Option<Duration> {
    periodic_reconcile_interval_from(
        std::env::var("UNEARTH_WATCH_RECONCILE_SECS")
            .ok()
            .as_deref(),
    )
}

fn periodic_reconcile_interval_from(value: Option<&str>) -> Option<Duration> {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|seconds| (seconds > 0).then_some(Duration::from_secs(seconds)))
}

pub(crate) fn covers_root(conn: &Connection, root_key: &str) -> Result<bool, String> {
    refresh_dead_watchers(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT root FROM watch_state
             WHERE status = 'running' AND dirty = 0 AND online = 1",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    for row in rows {
        let root = row.map_err(|e| e.to_string())?;
        if root == root_key
            || root == "/"
            || root_key.starts_with(&format!("{}/", root.trim_end_matches('/')))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn print_status() -> Result<(), String> {
    let conn = open_index_db()?;
    refresh_dead_watchers(&conn)?;
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
            last_event.map_or_else(|| "-".to_string(), |v| v.to_string()),
            last_reconcile.map_or_else(|| "-".to_string(), |v| v.to_string()),
            dirty,
            online,
            pid.map_or_else(|| "-".to_string(), |v| v.to_string()),
            heartbeat.map_or_else(|| "-".to_string(), |v| v.to_string()),
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

fn refresh_dead_watchers(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT root, watcher_pid, status, owner_boot_id, owner_starttime
             FROM watch_state WHERE online=1",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut dead = Vec::new();
    for row in rows {
        let (root, pid, status, boot_id, starttime) = row.map_err(|e| e.to_string())?;
        let alive = owner_is_alive(pid, boot_id.as_deref(), starttime);
        if !alive && matches!(status.as_str(), "starting" | "running" | "recovering") {
            dead.push(root);
        }
    }
    drop(stmt);
    for root in dead {
        conn.execute(
            "UPDATE watch_state SET status='stopped', online=0,
             error='watcher process is not running' WHERE root=?1",
            [root],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn run(opts: &Options) -> Result<(), String> {
    if opts.positional.is_empty() {
        return Err("--watch requires at least one directory root".to_string());
    }
    let mut roots = Vec::new();
    for raw in &opts.positional {
        let path = fs::canonicalize(expand_home_path(raw))
            .map_err(|e| format!("cannot watch '{}': {}", raw, e))?;
        if !path.is_dir() {
            return Err(format!("--watch target '{}' is not a directory", raw));
        }
        let key = normalize_index_dir(&path);
        if !roots.iter().any(|root: &RootState| root.key == key) {
            roots.push(RootState { key, path });
        }
    }
    if roots.is_empty() {
        return Err("--watch did not receive a usable directory root".to_string());
    }

    #[cfg(unix)]
    {
        WATCH_STOP.store(false, Ordering::Relaxed);
        install_watch_signal_handlers();
    }

    claim_watch_states(&roots)?;
    let threads = opts.threads_override.max(1);
    let _ = ThreadPoolBuilder::new().num_threads(threads).build_global();
    let (events, backend, backend_error) = match start_backend(&roots) {
        Ok(result) => result,
        Err(error) => {
            for root in &roots {
                let _ = update_state_error(&root.key, "none", &error);
            }
            return Err(error);
        }
    };
    if let Some(error) = backend_error {
        eprintln!("unearth: fanotify unavailable, using inotify: {}", error);
    }
    update_states(&roots, &backend, "starting", false, true, None, false)?;
    let mut event_conn = open_index_db()?;

    let mut initial_error = None;
    for root in &roots {
        if let Err(error) = refresh_index_root(&root.key, opts) {
            initial_error = Some(format!("{}: {}", root.key, error));
            update_state_error(&root.key, &backend, &error)?;
        } else {
            mark_reconciled(&root.key, &backend)?;
        }
    }
    if let Some(error) = initial_error {
        return Err(format!("initial live index scan failed: {}", error));
    }
    let mut startup_events = Vec::new();
    while let Ok(event) = events.try_recv() {
        startup_events.push(event);
        if startup_events.len() >= WATCH_BATCH_MAX {
            break;
        }
    }
    if !startup_events.is_empty() {
        if let Err(error) = process_batch(&roots, &backend, opts, startup_events, &mut event_conn) {
            for root in &roots {
                let _ = update_state_error(&root.key, &backend, &error);
            }
            return Err(format!("initial live event replay failed: {error}"));
        }
    }
    update_states(&roots, &backend, "running", false, true, None, false)?;
    eprintln!(
        "unearth: live index watching {} root{} with {}",
        roots.len(),
        if roots.len() == 1 { "" } else { "s" },
        backend
    );

    let mut last_heartbeat = std::time::Instant::now();
    let reconcile_interval = periodic_reconcile_interval();
    let mut last_periodic_reconcile = std::time::Instant::now();
    if reconcile_interval.is_none() {
        eprintln!("unearth: periodic reconciliation disabled; recovery scans remain event-driven");
    }
    loop {
        #[cfg(unix)]
        if WATCH_STOP.load(Ordering::Relaxed) {
            break;
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(5) {
            heartbeat_states(&roots, &backend)?;
            last_heartbeat = std::time::Instant::now();
        }
        if reconcile_interval.is_some_and(|interval| last_periodic_reconcile.elapsed() >= interval)
        {
            for root in &roots {
                if let Err(error) = reconcile_subtree(&root.path, opts) {
                    eprintln!(
                        "unearth: periodic reconciliation of {} failed: {error}",
                        root.key
                    );
                    let _ = update_state_error(&root.key, &backend, &error);
                } else {
                    let _ = mark_reconciled(&root.key, &backend);
                }
            }
            last_periodic_reconcile = std::time::Instant::now();
        }
        let first = match events.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => event,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                shutdown_states(&roots);
                return Err("live watcher backend stopped".to_string());
            }
        };
        let mut batch = vec![first];
        let deadline = std::time::Instant::now() + WATCH_BATCH_DELAY;
        while batch.len() < WATCH_BATCH_MAX {
            match events.try_recv() {
                Ok(event) => batch.push(event),
                Err(TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(WATCH_POLL_DELAY);
                }
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Err(error) = process_batch(&roots, &backend, opts, batch, &mut event_conn) {
            eprintln!("unearth: live event batch failed: {error}; retaining watcher");
            for root in &roots {
                let _ = update_state_error(&root.key, &backend, &error);
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
    shutdown_states(&roots);
    Ok(())
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn path_in_root(root: &str, path: &Path) -> bool {
    let path = normalize_index_dir(path);
    path == root || root == "/" && path.starts_with('/') || path.starts_with(&format!("{root}/"))
}

fn matching_root<'a>(roots: &'a [RootState], path: &Path) -> Option<&'a RootState> {
    roots
        .iter()
        .filter(|root| path_in_root(&root.key, path))
        .max_by_key(|root| root.key.len())
}

fn update_states(
    roots: &[RootState],
    backend: &str,
    status: &str,
    dirty: bool,
    online: bool,
    error: Option<&str>,
    reconcile: bool,
) -> Result<(), String> {
    let owner = current_owner();
    let conn = open_index_db()?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let now = now_seconds();
    for root in roots {
        tx.execute(
            "INSERT INTO watch_state(root, backend, status, generation, last_event,
                                    last_reconcile, dirty, online, watcher_pid, error,
                                    owner_boot_id, owner_starttime, heartbeat)
             VALUES (?1, ?2, ?3, 0, NULL, ?4, ?5, ?6, ?7, ?8, ?10, ?11, ?12)
             ON CONFLICT(root) DO UPDATE SET backend=excluded.backend,
                 status=excluded.status, last_reconcile=CASE WHEN ?9 THEN excluded.last_reconcile
                 ELSE watch_state.last_reconcile END, dirty=excluded.dirty,
                 online=excluded.online, watcher_pid=excluded.watcher_pid, error=excluded.error,
                 owner_boot_id=excluded.owner_boot_id, owner_starttime=excluded.owner_starttime,
                 heartbeat=excluded.heartbeat",
            params![
                root.key,
                backend,
                status,
                if reconcile { Some(now) } else { None },
                i64::from(dirty),
                i64::from(online),
                std::process::id() as i64,
                error,
                reconcile,
                owner.boot_id,
                owner.starttime,
                now,
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

fn update_state_error(root: &str, backend: &str, error: &str) -> Result<(), String> {
    let owner = current_owner();
    let conn = open_index_db()?;
    conn.execute(
        "INSERT INTO watch_state(root, backend, status, generation, dirty, online,
                                 watcher_pid, error, owner_boot_id, owner_starttime, heartbeat)
         VALUES (?1, ?2, 'error', 0, 1, 0, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(root) DO UPDATE SET backend=excluded.backend, status='error',
             dirty=1, online=0, watcher_pid=excluded.watcher_pid, error=excluded.error,
             owner_boot_id=excluded.owner_boot_id, owner_starttime=excluded.owner_starttime,
             heartbeat=excluded.heartbeat",
        params![
            root,
            backend,
            owner.pid,
            error,
            owner.boot_id,
            owner.starttime,
            now_seconds()
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn mark_reconciled(root: &str, backend: &str) -> Result<(), String> {
    let owner = current_owner();
    let conn = open_index_db()?;
    conn.execute(
        "INSERT INTO watch_state(root, backend, status, generation, last_reconcile,
                                 dirty, online, watcher_pid, error,
                                 owner_boot_id, owner_starttime, heartbeat)
         VALUES (?1, ?2, 'running', 0, ?3, 0, 1, ?4, NULL, ?5, ?6, ?3)
         ON CONFLICT(root) DO UPDATE SET backend=excluded.backend, status='running',
             last_reconcile=excluded.last_reconcile, dirty=0, online=1,
             watcher_pid=excluded.watcher_pid, error=NULL,
             owner_boot_id=excluded.owner_boot_id, owner_starttime=excluded.owner_starttime,
             heartbeat=excluded.heartbeat",
        params![
            root,
            backend,
            now_seconds(),
            owner.pid,
            owner.boot_id,
            owner.starttime
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn process_batch(
    roots: &[RootState],
    backend: &str,
    opts: &Options,
    batch: Vec<FsEvent>,
    conn: &mut Connection,
) -> Result<(), String> {
    let batch = coalesce_batch(batch);
    let mut full_refresh = HashSet::<String>::new();
    let mut reconcile = HashSet::<PathBuf>::new();
    let mut touched_roots = HashSet::<String>::new();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let now = now_seconds();
    for event in &batch {
        let root = matching_root(roots, &event.path);
        if root.is_none() && event.action != Action::Overflow {
            continue;
        }
        if root.is_none() && event.action == Action::Overflow {
            for root in roots {
                full_refresh.insert(root.key.clone());
                tx.execute(
                    "UPDATE watch_state SET dirty=1, status='recovering', last_event=?2,
                     generation=generation+1 WHERE root=?1",
                    params![root.key, now],
                )
                .map_err(|e| e.to_string())?;
            }
            continue;
        }
        let Some(root) = root else {
            continue;
        };
        for candidate in roots {
            if path_in_root(&candidate.key, &event.path) {
                touched_roots.insert(candidate.key.clone());
            }
            if let Some(old_path) = event.old_path.as_deref() {
                if path_in_root(&candidate.key, old_path) {
                    touched_roots.insert(candidate.key.clone());
                }
            }
        }
        if is_root_index_excluded_path(&root.key, &normalize_index_dir(&event.path)) {
            continue;
        }
        match event.action {
            Action::Overflow => {
                if event.path != Path::new("/") && event.path != root.path {
                    reconcile.insert(event.path.clone());
                } else {
                    full_refresh.insert(root.key.clone());
                }
                tx.execute(
                    "UPDATE watch_state SET dirty=1, status='recovering', last_event=?2,
                     generation=generation+1 WHERE root=?1",
                    params![root.key, now],
                )
                .map_err(|e| e.to_string())?;
            }
            Action::Reconcile => {
                reconcile.insert(event.path.clone());
                touched_roots.insert(root.key.clone());
            }
            Action::Upsert => {
                upsert_path(
                    &tx,
                    &event.path,
                    event.is_dir,
                    &event.actor,
                    event.event_kind,
                )?;
                touch_state(&tx, &root.key, event.at)?;
                touched_roots.insert(root.key.clone());
            }
            Action::Remove => {
                remove_path(&tx, &event.path, event.is_dir)?;
                touch_state(&tx, &root.key, event.at)?;
                touched_roots.insert(root.key.clone());
            }
            Action::Move => {
                if let Some(old_path) = event.old_path.as_deref() {
                    remove_path(&tx, old_path, event.is_dir)?;
                }
                upsert_path(
                    &tx,
                    &event.path,
                    event.is_dir,
                    &event.actor,
                    event.event_kind,
                )?;
                if event.is_dir {
                    reconcile.insert(event.path.clone());
                }
                touch_state(&tx, &root.key, event.at)?;
                touched_roots.insert(root.key.clone());
            }
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    for path in reconcile {
        if path.is_dir() {
            if let Err(error) = reconcile_subtree(&path, opts) {
                if let Some(root) = matching_root(roots, &path) {
                    update_state_error(&root.key, backend, &error)?;
                    full_refresh.insert(root.key.clone());
                }
            } else if let Some(root) = matching_root(roots, &path) {
                mark_reconciled(&root.key, backend)?;
            }
        }
    }
    for root in full_refresh {
        if let Err(error) = refresh_index_root(&root, opts) {
            update_state_error(&root, backend, &error)?;
        } else {
            mark_reconciled(&root, backend)?;
        }
    }
    Ok(())
}

fn coalesce_batch(batch: Vec<FsEvent>) -> Vec<FsEvent> {
    let mut result = Vec::with_capacity(batch.len());
    let mut positions = HashMap::<PathBuf, usize>::new();
    for event in batch {
        if matches!(event.action, Action::Move) {
            result.push(event);
            continue;
        }
        let path = event.path.clone();
        if let Some(index) = positions.get(&path).copied() {
            let previous = &result[index];
            let replace = match (previous.action, event.action) {
                (Action::Reconcile, Action::Upsert) => false,
                (_, Action::Reconcile) => true,
                _ => true,
            };
            if replace {
                result[index] = event;
            }
        } else {
            positions.insert(path, result.len());
            result.push(event);
        }
    }
    result
}

fn touch_state(tx: &Transaction<'_>, root: &str, at: i64) -> Result<(), String> {
    tx.execute(
        "UPDATE watch_state SET last_event=?2, generation=generation+1,
         status='running', dirty=0, online=1, error=NULL WHERE root=?1",
        params![root, at / 1_000_000_000],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn actor_id(tx: &Transaction<'_>, actor: &Actor, at: i64) -> Result<i64, String> {
    tx.execute(
        "INSERT INTO actors(executable, classification, first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(executable) DO UPDATE SET classification=excluded.classification,
             last_seen=excluded.last_seen",
        params![actor.executable, actor.classification, at / 1_000_000_000],
    )
    .map_err(|e| e.to_string())?;
    tx.query_row(
        "SELECT id FROM actors WHERE executable=?1",
        [&actor.executable],
        |row| row.get(0),
    )
    .map_err(|e| e.to_string())
}

fn ensure_dir(tx: &Transaction<'_>, path: &str) -> Result<i64, String> {
    tx.execute("INSERT OR IGNORE INTO dirs(path) VALUES (?1)", [path])
        .map_err(|e| e.to_string())?;
    tx.query_row("SELECT id FROM dirs WHERE path=?1", [path], |row| {
        row.get(0)
    })
    .map_err(|e| e.to_string())
}

fn ensure_dir_chain(tx: &Transaction<'_>, path: &Path) -> Result<i64, String> {
    let key = normalize_index_dir(path);
    let mut current = PathBuf::from("/");
    ensure_dir(tx, "/")?;
    if key != "/" {
        for component in Path::new(&key).components() {
            if let Component::Normal(part) = component {
                current.push(part);
                ensure_dir(tx, &normalize_index_dir(&current))?;
            }
        }
    }
    ensure_dir(tx, &key)
}

fn ensure_name(tx: &Transaction<'_>, name: &str) -> Result<i64, String> {
    tx.execute("INSERT OR IGNORE INTO strings(value) VALUES (?1)", [name])
        .map_err(|e| e.to_string())?;
    tx.query_row("SELECT id FROM strings WHERE value=?1", [name], |row| {
        row.get(0)
    })
    .map_err(|e| e.to_string())
}

fn upsert_path(
    tx: &Transaction<'_>,
    raw_path: &Path,
    is_dir_hint: bool,
    actor: &Actor,
    event_kind: i64,
) -> Result<(), String> {
    let path = normalize_index_dir(raw_path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_path(tx, raw_path, is_dir_hint)?;
            return Ok(());
        }
        Err(error) => return Err(error.to_string()),
    };
    let is_dir = metadata.file_type().is_dir() || is_dir_hint && path.ends_with('/');
    let kind = if is_dir {
        1
    } else if metadata.file_type().is_symlink() {
        2
    } else {
        0
    };
    let parent = Path::new(&path)
        .parent()
        .ok_or_else(|| "event path has no parent".to_string())?;
    let name = Path::new(&path)
        .file_name()
        .ok_or_else(|| "event path has no basename".to_string())?
        .to_string_lossy()
        .into_owned();
    let dir_id = ensure_dir_chain(tx, parent)?;
    let name_id = ensure_name(tx, &name)?;
    let actor_id = actor_id(tx, actor, now_nanos())?;
    tx.execute(
        "DELETE FROM entries WHERE dir_id=?1 AND name_id=?2 AND kind<>?3",
        params![dir_id, name_id, kind],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO entries(dir_id, name_id, kind, mtime, size, activity,
                             event_kind, actor_id, actor_uid, actor_pid, event_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(dir_id, name_id, kind) DO UPDATE SET mtime=excluded.mtime,
             size=excluded.size, activity=excluded.activity, event_kind=excluded.event_kind,
             actor_id=CASE WHEN excluded.event_kind = 6
                                OR excluded.actor_id = (SELECT id FROM actors WHERE executable = 'unknown')
                           THEN entries.actor_id ELSE excluded.actor_id END,
             actor_uid=CASE WHEN excluded.event_kind = 6
                                 OR excluded.actor_id = (SELECT id FROM actors WHERE executable = 'unknown')
                            THEN entries.actor_uid ELSE excluded.actor_uid END,
             actor_pid=CASE WHEN excluded.event_kind = 6
                                 OR excluded.actor_id = (SELECT id FROM actors WHERE executable = 'unknown')
                            THEN entries.actor_pid ELSE excluded.actor_pid END,
             event_at=CASE WHEN excluded.event_kind = 6
                                OR excluded.actor_id = (SELECT id FROM actors WHERE executable = 'unknown')
                           THEN entries.event_at ELSE excluded.event_at END",
        params![
            dir_id,
            name_id,
            kind,
            metadata_mtime_nanos(&metadata),
            metadata_size_i64(&metadata),
            metadata_activity_nanos(&metadata),
            event_kind,
            actor_id,
            actor.uid,
            actor.pid,
            now_nanos(),
        ],
    )
    .map_err(|e| e.to_string())?;
    if is_dir {
        ensure_dir(tx, &path)?;
    }
    Ok(())
}

fn remove_path(tx: &Transaction<'_>, raw_path: &Path, is_dir_hint: bool) -> Result<(), String> {
    let path = normalize_index_dir(raw_path);
    if path == "/" {
        return Ok(());
    }
    let parent = Path::new(&path)
        .parent()
        .ok_or_else(|| "removed path has no parent".to_string())?;
    let name = Path::new(&path)
        .file_name()
        .ok_or_else(|| "removed path has no basename".to_string())?
        .to_string_lossy()
        .into_owned();
    let parent_key = normalize_index_dir(parent);
    let name_id: Option<i64> = tx
        .query_row("SELECT id FROM strings WHERE value=?1", [&name], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    let parent_id: Option<i64> = tx
        .query_row("SELECT id FROM dirs WHERE path=?1", [&parent_key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    let escaped_path = sql_like_escape(&path);
    let subtree_pattern = format!("{escaped_path}/%");
    let is_dir = is_dir_hint
        || tx
            .query_row(
                "SELECT COUNT(*) FROM dirs WHERE path=?1 OR path LIKE ?2 ESCAPE '\\'",
                params![path, &subtree_pattern],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| e.to_string())?
            > 0;
    if is_dir {
        tx.execute(
            "DELETE FROM entries WHERE dir_id IN
             (SELECT id FROM dirs WHERE path=?1 OR path LIKE ?2 ESCAPE '\\')
             OR (dir_id=?3 AND name_id=?4)",
            params![path, &subtree_pattern, parent_id, name_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM dirs WHERE path=?1 OR path LIKE ?2 ESCAPE '\\'",
            params![path, &subtree_pattern],
        )
        .map_err(|e| e.to_string())?;
    } else if let (Some(parent_id), Some(name_id)) = (parent_id, name_id) {
        tx.execute(
            "DELETE FROM entries WHERE dir_id=?1 AND name_id=?2",
            params![parent_id, name_id],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn reconcile_subtree(path: &Path, opts: &Options) -> Result<(), String> {
    let path = path.to_path_buf();
    if !path.is_dir() {
        return Ok(());
    }
    let root_key = normalize_index_dir(&path);
    let threads = if root_prefers_single_thread(&path) {
        1
    } else {
        opts.threads_override.max(1)
    };
    let entries = scan_index_root(&path, &root_key, threads)?;
    let mut conn = open_index_db()?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM entries WHERE dir_id IN
         (SELECT id FROM dirs WHERE path=?1 OR path LIKE ?2 ESCAPE '\\')",
        params![root_key, format!("{}/%", sql_like_escape(&root_key))],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM dirs WHERE path LIKE ?1 ESCAPE '\\'",
        [format!("{}/%", sql_like_escape(&root_key))],
    )
    .map_err(|e| e.to_string())?;
    ensure_dir(&tx, &root_key)?;
    let actor = Actor::reconcile();
    for entry in entries {
        let path = PathBuf::from(entry.path);
        let metadata = fs::symlink_metadata(&path).ok();
        if metadata.is_some() {
            upsert_path(&tx, &path, entry.kind == 1, &actor, EVENT_RECONCILE)?;
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

fn start_backend(
    roots: &[RootState],
) -> Result<(Receiver<FsEvent>, String, Option<String>), String> {
    match start_fanotify(roots) {
        Ok((rx, name)) => Ok((rx, name, None)),
        Err(fanotify_error) => {
            let rx = start_inotify(roots)?;
            Ok((rx, "inotify".to_string(), Some(fanotify_error)))
        }
    }
}

fn event(action: Action, path: PathBuf, is_dir: bool, _backend: &str, actor: Actor) -> FsEvent {
    FsEvent {
        action,
        path,
        old_path: None,
        is_dir,
        actor,
        event_kind: match action {
            Action::Upsert => EVENT_CREATE,
            Action::Remove => EVENT_DELETE,
            Action::Move => EVENT_MOVE,
            Action::Reconcile => EVENT_RECONCILE,
            Action::Overflow => EVENT_OVERFLOW,
        },
        at: now_nanos(),
    }
}

fn classify_actor(pid: i32) -> Actor {
    if pid <= 0 {
        return Actor::unknown();
    }
    let proc_root = PathBuf::from(format!("/proc/{pid}"));
    let executable = fs::read_link(proc_root.join("exe"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("pid:{pid}"));
    let uid = fs::read_to_string(proc_root.join("status"))
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                let rest = line.strip_prefix("Uid:")?.split_whitespace().next()?;
                rest.parse::<i64>().ok()
            })
        });
    let lower = executable.to_ascii_lowercase();
    let classification = if lower.contains("codex") {
        "codex"
    } else if uid == Some(0)
        || lower.contains("systemd")
        || lower.contains("packagekit")
        || lower.contains("pacman")
        || lower.contains("dnf")
        || lower.contains("apt")
    {
        "system"
    } else if [
        "/bash", "/zsh", "/fish", "/vim", "/nvim", "/micro", "/nano", "/kate", "/emacs", "/helix",
        "/hx", "/code",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
    {
        "manual"
    } else {
        "application"
    };
    Actor {
        executable,
        classification: classification.to_string(),
        uid,
        pid: Some(i64::from(pid)),
    }
}

#[cfg(target_os = "linux")]
fn start_inotify(roots: &[RootState]) -> Result<Receiver<FsEvent>, String> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut watcher = InotifyWatcher {
        fd,
        roots: roots.to_vec(),
        paths: HashMap::new(),
        path_to_wd: BTreeMap::new(),
        pending_moves: HashMap::new(),
        mounted: roots
            .iter()
            .flat_map(|root| mount_candidates(&root.path))
            .collect(),
        last_mount_check: std::time::Instant::now(),
    };
    for root in roots {
        if let Err(error) = watcher.add_recursive(&root.path) {
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
    }
    let (tx, rx) = bounded(WATCH_CHANNEL_CAPACITY);
    thread::Builder::new()
        .name("unearth-inotify".to_string())
        .spawn(move || watcher.run(tx))
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

#[cfg(not(target_os = "linux"))]
fn start_inotify(_roots: &[RootState]) -> Result<Receiver<FsEvent>, String> {
    Err("inotify is only available on Linux".to_string())
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct PendingMove {
    path: PathBuf,
    is_dir: bool,
    seen_at: std::time::Instant,
}

#[cfg(target_os = "linux")]
struct InotifyWatcher {
    fd: RawFd,
    roots: Vec<RootState>,
    paths: HashMap<i32, PathBuf>,
    path_to_wd: BTreeMap<PathBuf, i32>,
    pending_moves: HashMap<u32, PendingMove>,
    mounted: HashSet<PathBuf>,
    last_mount_check: std::time::Instant,
}

#[cfg(target_os = "linux")]
impl InotifyWatcher {
    fn expire_pending_moves(&mut self, tx: &Sender<FsEvent>) {
        let now = std::time::Instant::now();
        let expired: Vec<u32> = self
            .pending_moves
            .iter()
            .filter_map(|(cookie, pending)| {
                (now.duration_since(pending.seen_at) >= PENDING_MOVE_TIMEOUT).then_some(*cookie)
            })
            .collect();
        for cookie in expired {
            if let Some(pending) = self.pending_moves.remove(&cookie) {
                if pending.is_dir {
                    self.remove_watches_below(&pending.path);
                }
                let _ = tx.send(event(
                    Action::Remove,
                    pending.path,
                    pending.is_dir,
                    "inotify",
                    Actor::unknown(),
                ));
            }
        }
    }

    fn add_watch(&mut self, path: &Path) -> Result<(), String> {
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("cannot watch path containing NUL: {}", path.display()))?;
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_UNMOUNT
            | libc::IN_Q_OVERFLOW
            | libc::IN_IGNORED;
        let wd = unsafe { libc::inotify_add_watch(self.fd, name.as_ptr(), mask) };
        if wd < 0 {
            let error = std::io::Error::last_os_error();
            if transient_watch_path_error(&error) {
                return Ok(());
            }
            if error.raw_os_error() == Some(libc::ENOSPC) {
                return Err(
                    "inotify watch limit reached; use a privileged fanotify watcher or raise fs.inotify.max_user_watches"
                        .to_string(),
                );
            }
            return Err(format!(
                "cannot add inotify watch for {}: {}",
                path.display(),
                error
            ));
        }
        self.paths.insert(wd, path.to_path_buf());
        self.path_to_wd.insert(path.to_path_buf(), wd);
        Ok(())
    }

    fn add_recursive(&mut self, root: &Path) -> Result<(), String> {
        let root_key = normalize_index_dir(root);
        for entry in WalkDir::new(root)
            .skip_hidden(false)
            .parallelism(Parallelism::Serial)
            .process_read_dir({
                let root_key = root_key.clone();
                move |_depth, _path, _state, children| {
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
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.io_error().is_some_and(transient_watch_path_error) => {
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            };
            let path = entry.path();
            if !entry.file_type().is_dir() {
                continue;
            }
            let path_key = normalize_index_dir(&path);
            let Some(root) = matching_root(&self.roots, &path) else {
                continue;
            };
            if is_root_index_excluded_path(&root.key, &path_key) {
                continue;
            }
            self.add_watch(&path)?;
        }
        Ok(())
    }

    fn remove_watches_below(&mut self, path: &Path) {
        let doomed: Vec<i32> = self
            .paths
            .iter()
            .filter_map(|(wd, watched)| watched.starts_with(path).then_some(*wd))
            .collect();
        for wd in doomed {
            if let Some(watched) = self.paths.remove(&wd) {
                self.path_to_wd.remove(&watched);
            }
            unsafe {
                libc::inotify_rm_watch(self.fd, wd);
            }
        }
    }

    fn update_watches_after_move(&mut self, old: &Path, new: &Path) {
        let upper = old.join("\u{10ffff}");
        let moved: Vec<(i32, PathBuf, PathBuf)> = self
            .path_to_wd
            .range(old.to_path_buf()..upper)
            .filter_map(|(path, wd)| {
                path.strip_prefix(old)
                    .ok()
                    .map(|suffix| (*wd, path.clone(), new.join(suffix)))
            })
            .collect();
        for (wd, old_path, new_path) in moved {
            self.path_to_wd.remove(&old_path);
            self.path_to_wd.insert(new_path.clone(), wd);
            if let Some(path) = self.paths.get_mut(&wd) {
                *path = new_path;
            }
        }
    }

    fn poll_mounts(&mut self, tx: &Sender<FsEvent>) {
        if self.last_mount_check.elapsed() < Duration::from_secs(2) {
            return;
        }
        let current: HashSet<PathBuf> = self
            .roots
            .iter()
            .flat_map(|root| mount_candidates(&root.path))
            .collect();
        let added: Vec<PathBuf> = current.difference(&self.mounted).cloned().collect();
        let removed: Vec<PathBuf> = self.mounted.difference(&current).cloned().collect();
        for mount in added {
            match self.add_recursive(&mount) {
                Ok(()) => {
                    let _ = tx.send(event(
                        Action::Reconcile,
                        mount,
                        true,
                        "inotify",
                        Actor::unknown(),
                    ));
                }
                Err(error) => {
                    eprintln!(
                        "unearth: cannot watch newly mounted {}: {error}",
                        mount.display()
                    );
                    let _ = tx.send(event(
                        Action::Overflow,
                        mount,
                        true,
                        "inotify",
                        Actor::unknown(),
                    ));
                }
            }
        }
        for mount in removed {
            let _ = tx.send(event(
                Action::Overflow,
                mount,
                true,
                "inotify",
                Actor::unknown(),
            ));
        }
        self.mounted = current;
        self.last_mount_check = std::time::Instant::now();
    }

    fn run(mut self, tx: Sender<FsEvent>) {
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            self.expire_pending_moves(&tx);
            self.poll_mounts(&tx);
            let read = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    thread::sleep(WATCH_POLL_DELAY);
                    continue;
                }
                let _ = tx.send(event(
                    Action::Overflow,
                    PathBuf::from("/"),
                    true,
                    "inotify",
                    Actor::unknown(),
                ));
                break;
            }
            if read == 0 {
                break;
            }
            let mut offset = 0usize;
            while offset + 16 <= read as usize {
                let wd = i32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap());
                let mask = u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
                let cookie =
                    u32::from_ne_bytes(buffer[offset + 8..offset + 12].try_into().unwrap());
                let len = u32::from_ne_bytes(buffer[offset + 12..offset + 16].try_into().unwrap())
                    as usize;
                let record_len = 16usize.saturating_add(len);
                if offset + record_len > read as usize {
                    let _ = tx.send(event(
                        Action::Overflow,
                        PathBuf::from("/"),
                        true,
                        "inotify",
                        Actor::unknown(),
                    ));
                    break;
                }
                let base = self.paths.get(&wd).cloned();
                if mask & libc::IN_IGNORED != 0 {
                    if let Some(ignored) = self.paths.remove(&wd) {
                        self.path_to_wd.remove(&ignored);
                    }
                    offset += record_len;
                    continue;
                }
                let name = buffer[offset + 16..offset + record_len]
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap_or_default();
                let path = base.map(|base| {
                    if name.is_empty() {
                        base
                    } else {
                        base.join(OsStr::from_bytes(name))
                    }
                });
                if mask & libc::IN_Q_OVERFLOW != 0 {
                    let _ = tx.send(event(
                        Action::Overflow,
                        PathBuf::from("/"),
                        true,
                        "inotify",
                        Actor::unknown(),
                    ));
                    offset += record_len;
                    continue;
                }
                let Some(path) = path else {
                    offset += record_len;
                    continue;
                };
                let is_dir = mask & libc::IN_ISDIR != 0;
                if mask & libc::IN_MOVED_FROM != 0 {
                    if cookie != 0 {
                        self.pending_moves.insert(
                            cookie,
                            PendingMove {
                                path,
                                is_dir,
                                seen_at: std::time::Instant::now(),
                            },
                        );
                    } else {
                        let _ = tx.send(event(
                            Action::Remove,
                            path,
                            is_dir,
                            "inotify",
                            Actor::unknown(),
                        ));
                    }
                } else if mask & libc::IN_MOVED_TO != 0 {
                    if let Some(old) = self.pending_moves.remove(&cookie) {
                        self.update_watches_after_move(&old.path, &path);
                        let mut moved = event(
                            Action::Move,
                            path.clone(),
                            is_dir || old.is_dir,
                            "inotify",
                            Actor::unknown(),
                        );
                        moved.old_path = Some(old.path);
                        let _ = tx.send(moved);
                    } else {
                        if let Err(error) = self.add_recursive(&path) {
                            eprintln!("unearth: cannot watch moved-in {}: {error}", path.display());
                        }
                        let _ = tx.send(event(
                            Action::Reconcile,
                            path.clone(),
                            is_dir,
                            "inotify",
                            Actor::unknown(),
                        ));
                        if !is_dir {
                            let _ = tx.send(event(
                                Action::Upsert,
                                path,
                                is_dir,
                                "inotify",
                                Actor::unknown(),
                            ));
                        }
                    }
                } else if mask & libc::IN_CREATE != 0 {
                    if is_dir {
                        if let Err(error) = self.add_recursive(&path) {
                            eprintln!(
                                "unearth: cannot watch new directory {}: {error}",
                                path.display()
                            );
                        }
                        // Install the watch before reconciling so children created
                        // immediately after mkdir cannot be missed.
                        let _ = tx.send(event(
                            Action::Reconcile,
                            path.clone(),
                            true,
                            "inotify",
                            Actor::unknown(),
                        ));
                    }
                    let _ = tx.send(event(
                        Action::Upsert,
                        path,
                        is_dir,
                        "inotify",
                        Actor::unknown(),
                    ));
                } else if mask & libc::IN_DELETE != 0 {
                    if is_dir {
                        self.remove_watches_below(&path);
                    }
                    let _ = tx.send(event(
                        Action::Remove,
                        path,
                        is_dir,
                        "inotify",
                        Actor::unknown(),
                    ));
                } else if mask & libc::IN_UNMOUNT != 0 {
                    let _ = tx.send(event(
                        Action::Overflow,
                        path,
                        true,
                        "inotify",
                        Actor::unknown(),
                    ));
                } else if mask & libc::IN_CLOSE_WRITE != 0 {
                    let mut changed =
                        event(Action::Upsert, path, is_dir, "inotify", Actor::unknown());
                    changed.event_kind = EVENT_MODIFY;
                    let _ = tx.send(changed);
                } else if mask & libc::IN_ATTRIB != 0 {
                    let mut changed =
                        event(Action::Upsert, path, is_dir, "inotify", Actor::unknown());
                    changed.event_kind = EVENT_ATTRIB;
                    let _ = tx.send(changed);
                } else if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
                    let _ = tx.send(event(
                        Action::Overflow,
                        path,
                        is_dir,
                        "inotify",
                        Actor::unknown(),
                    ));
                }
                offset += record_len;
            }
        }
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn start_fanotify(_roots: &[RootState]) -> Result<(Receiver<FsEvent>, String), String> {
    Err("fanotify is only available on Linux".to_string())
}

#[cfg(target_os = "linux")]
fn start_fanotify(roots: &[RootState]) -> Result<(Receiver<FsEvent>, String), String> {
    let flags = libc::FAN_CLOEXEC | libc::FAN_NONBLOCK | libc::FAN_REPORT_DFID_NAME;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_fanotify_init,
            flags as libc::c_uint,
            (libc::O_RDONLY | libc::O_LARGEFILE) as libc::c_uint,
        ) as i32
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut mounts = Vec::new();
    let mut marked = HashSet::new();
    let mut mount_ids = HashMap::new();
    for root in roots {
        let candidates = mount_candidates(&root.path);
        for mount in candidates {
            if !marked.insert(mount.clone()) {
                continue;
            }
            let Some(c_path) = CString::new(mount.as_os_str().as_bytes()).ok() else {
                continue;
            };
            let result = unsafe {
                libc::syscall(
                    libc::SYS_fanotify_mark,
                    fd,
                    (libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM) as libc::c_uint,
                    fanotify_event_mask() as libc::c_ulong,
                    libc::AT_FDCWD,
                    c_path.as_ptr(),
                ) as i32
            };
            if result < 0 {
                unsafe {
                    libc::close(fd);
                }
                return Err(format!(
                    "cannot mark {}: {}",
                    mount.display(),
                    std::io::Error::last_os_error()
                ));
            }
            let open_fd = unsafe {
                libc::open(
                    c_path.as_ptr(),
                    libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if open_fd >= 0 {
                mounts.push(MountHandle {
                    fd: open_fd,
                    path: mount.clone(),
                });
                mount_ids.insert(mount.clone(), mount_identity(&mount));
            }
        }
    }
    if mounts.is_empty() {
        unsafe {
            libc::close(fd);
        }
        return Err("no fanotify mount could be opened".to_string());
    }
    let (tx, rx) = bounded(WATCH_CHANNEL_CAPACITY);
    let roots = roots.to_vec();
    thread::Builder::new()
        .name("unearth-fanotify".to_string())
        .spawn(move || {
            FanotifyWatcher {
                fd,
                roots,
                mounts,
                marked,
                fsid_mounts: HashMap::new(),
                mount_ids,
            }
            .run(tx)
        })
        .map_err(|e| e.to_string())?;
    Ok((rx, "fanotify".to_string()))
}

#[cfg(target_os = "linux")]
fn fanotify_event_mask() -> u64 {
    libc::FAN_CREATE
        | libc::FAN_DELETE
        | libc::FAN_MOVED_FROM
        | libc::FAN_MOVED_TO
        | libc::FAN_RENAME
        | libc::FAN_MODIFY
        | libc::FAN_CLOSE_WRITE
        | libc::FAN_ATTRIB
        | libc::FAN_DELETE_SELF
        | libc::FAN_MOVE_SELF
        | libc::FAN_EVENT_ON_CHILD
        | libc::FAN_ONDIR
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct MountHandle {
    fd: RawFd,
    path: PathBuf,
}

#[cfg(target_os = "linux")]
struct FanotifyWatcher {
    fd: RawFd,
    roots: Vec<RootState>,
    mounts: Vec<MountHandle>,
    marked: HashSet<PathBuf>,
    fsid_mounts: HashMap<[u8; 8], usize>,
    mount_ids: HashMap<PathBuf, Option<u64>>,
}

#[cfg(target_os = "linux")]
impl FanotifyWatcher {
    fn add_new_mounts(&mut self, tx: &Sender<FsEvent>) {
        let current_ids: HashMap<PathBuf, Option<u64>> = self
            .roots
            .iter()
            .flat_map(|root| mount_candidates(&root.path))
            .map(|mount| {
                let identity = mount_identity(&mount);
                (mount, identity)
            })
            .collect();
        let stale: Vec<PathBuf> = self
            .marked
            .iter()
            .filter(|mount| self.mount_ids.get(*mount) != current_ids.get(*mount))
            .cloned()
            .collect();
        for mount in stale {
            if let Ok(c_path) = CString::new(mount.as_os_str().as_bytes()) {
                unsafe {
                    libc::syscall(
                        libc::SYS_fanotify_mark,
                        self.fd,
                        (libc::FAN_MARK_REMOVE | libc::FAN_MARK_FILESYSTEM) as libc::c_uint,
                        fanotify_event_mask() as libc::c_ulong,
                        libc::AT_FDCWD,
                        c_path.as_ptr(),
                    );
                }
            }
            for handle in self.mounts.iter().filter(|handle| handle.path == mount) {
                unsafe {
                    libc::close(handle.fd);
                }
            }
            self.mounts.retain(|handle| handle.path != mount);
            self.marked.remove(&mount);
            self.fsid_mounts.clear();
            self.mount_ids.remove(&mount);
        }
        for root in &self.roots {
            for mount in mount_candidates(&root.path) {
                if self.marked.contains(&mount)
                    && self.mount_ids.get(&mount) == current_ids.get(&mount)
                {
                    continue;
                }
                let Some(c_path) = CString::new(mount.as_os_str().as_bytes()).ok() else {
                    continue;
                };
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_fanotify_mark,
                        self.fd,
                        (libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM) as libc::c_uint,
                        fanotify_event_mask() as libc::c_ulong,
                        libc::AT_FDCWD,
                        c_path.as_ptr(),
                    ) as i32
                };
                if result < 0 {
                    eprintln!(
                        "unearth: cannot mark newly mounted {}: {}",
                        mount.display(),
                        std::io::Error::last_os_error()
                    );
                    continue;
                }
                let mount_fd = unsafe {
                    libc::open(
                        c_path.as_ptr(),
                        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    )
                };
                if mount_fd >= 0 {
                    self.marked.insert(mount.clone());
                    self.mounts.push(MountHandle {
                        fd: mount_fd,
                        path: mount.clone(),
                    });
                    self.mount_ids
                        .insert(mount.clone(), current_ids.get(&mount).copied().flatten());
                    self.fsid_mounts.clear();
                    let _ = tx.send(event(
                        Action::Reconcile,
                        mount,
                        true,
                        "fanotify",
                        Actor::unknown(),
                    ));
                } else {
                    eprintln!(
                        "unearth: cannot open newly mounted {}: {}",
                        mount.display(),
                        std::io::Error::last_os_error()
                    );
                }
            }
        }
    }

    fn resolve_handle(&mut self, record: &[u8]) -> Option<PathBuf> {
        if record.len() < 20 {
            return None;
        }
        let fsid: [u8; 8] = record[4..12].try_into().ok()?;
        let bytes = u32::from_ne_bytes(record[12..16].try_into().ok()?) as usize;
        if record.len() < 20 + bytes {
            return None;
        }
        let mut handle = Vec::with_capacity(8 + bytes);
        handle.extend_from_slice(&(bytes as u32).to_ne_bytes());
        handle.extend_from_slice(&record[16..20]);
        handle.extend_from_slice(&record[20..20 + bytes]);
        let mut candidates = self
            .fsid_mounts
            .get(&fsid)
            .copied()
            .map(|index| vec![index])
            .unwrap_or_else(|| (0..self.mounts.len()).collect());
        candidates.retain(|index| *index < self.mounts.len());
        for index in candidates {
            let mount = &self.mounts[index];
            let handle_ptr = handle.as_mut_ptr().cast::<libc::file_handle>();
            let resolved = unsafe {
                libc::syscall(
                    libc::SYS_open_by_handle_at,
                    mount.fd,
                    handle_ptr,
                    (libc::O_PATH | libc::O_CLOEXEC) as libc::c_int,
                ) as i32
            };
            if resolved < 0 {
                continue;
            }
            let link = fs::read_link(format!("/proc/self/fd/{resolved}"));
            unsafe {
                libc::close(resolved);
            }
            if let Ok(path) = link {
                let path = PathBuf::from(path.to_string_lossy().trim_end_matches(" (deleted)"));
                self.fsid_mounts.insert(fsid, index);
                // Filesystem marks also deliver events outside the requested
                // logical roots. Return the resolved path so the consumer can
                // discard those events without treating them as corruption.
                return Some(path);
            }
        }
        None
    }

    fn parse_info_records(
        &mut self,
        data: &[u8],
    ) -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
        if data.len() < 24 {
            return (None, None, None);
        }
        let metadata_len = u16::from_ne_bytes(data[6..8].try_into().unwrap()) as usize;
        if !(24..=data.len()).contains(&metadata_len) {
            return (None, None, None);
        }
        let mut offset = metadata_len;
        let mut target = None;
        let mut old = None;
        let mut new = None;
        while offset + 4 <= data.len() {
            let info_type = data[offset];
            let len = u16::from_ne_bytes(data[offset + 2..offset + 4].try_into().unwrap()) as usize;
            if len < 4 || offset + len > data.len() {
                break;
            }
            let record = &data[offset..offset + len];
            match info_type {
                libc::FAN_EVENT_INFO_TYPE_FID
                | libc::FAN_EVENT_INFO_TYPE_DFID_NAME
                | libc::FAN_EVENT_INFO_TYPE_DFID => {
                    let resolved = self.resolve_handle(record);
                    if info_type == libc::FAN_EVENT_INFO_TYPE_DFID_NAME && resolved.is_some() {
                        let bytes = u32::from_ne_bytes(record[12..16].try_into().unwrap()) as usize;
                        if record.len() > 20 + bytes {
                            let name = &record[20 + bytes..];
                            let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
                            target = resolved.map(|parent| parent.join(OsStr::from_bytes(name)));
                        }
                    } else {
                        target = resolved;
                    }
                }
                libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME => {
                    let resolved = self.resolve_handle(record);
                    if let Some(parent) = resolved {
                        let bytes = u32::from_ne_bytes(record[12..16].try_into().unwrap()) as usize;
                        if record.len() > 20 + bytes {
                            let name = &record[20 + bytes..];
                            let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
                            old = Some(parent.join(OsStr::from_bytes(name)));
                        }
                    }
                }
                libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME => {
                    let resolved = self.resolve_handle(record);
                    if let Some(parent) = resolved {
                        let bytes = u32::from_ne_bytes(record[12..16].try_into().unwrap()) as usize;
                        if record.len() > 20 + bytes {
                            let name = &record[20 + bytes..];
                            let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
                            new = Some(parent.join(OsStr::from_bytes(name)));
                        }
                    }
                }
                _ => {}
            }
            let next = (offset + len + 7) & !7;
            if next <= offset {
                break;
            }
            offset = next;
        }
        (target, old, new)
    }

    fn run(mut self, tx: Sender<FsEvent>) {
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut last_mount_check = std::time::Instant::now();
        loop {
            if last_mount_check.elapsed() >= Duration::from_secs(2) {
                self.add_new_mounts(&tx);
                last_mount_check = std::time::Instant::now();
            }
            let read = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    thread::sleep(WATCH_POLL_DELAY);
                    continue;
                }
                let _ = tx.send(event(
                    Action::Overflow,
                    PathBuf::from("/"),
                    true,
                    "fanotify",
                    Actor::unknown(),
                ));
                break;
            }
            if read == 0 {
                break;
            }
            let mut offset = 0usize;
            while offset + 24 <= read as usize {
                let event_len =
                    u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
                if event_len < 24 || offset + event_len > read as usize {
                    let _ = tx.send(event(
                        Action::Overflow,
                        self.roots
                            .first()
                            .map(|root| root.path.clone())
                            .unwrap_or_else(|| PathBuf::from("/")),
                        true,
                        "fanotify",
                        Actor::unknown(),
                    ));
                    break;
                }
                let metadata_version = u8::from_ne_bytes([buffer[offset + 4]]);
                let metadata_len =
                    u16::from_ne_bytes(buffer[offset + 6..offset + 8].try_into().unwrap()) as usize;
                if metadata_version != libc::FANOTIFY_METADATA_VERSION
                    || metadata_len < 24
                    || metadata_len > event_len
                {
                    let _ = tx.send(event(
                        Action::Overflow,
                        self.roots
                            .first()
                            .map(|root| root.path.clone())
                            .unwrap_or_else(|| PathBuf::from("/")),
                        true,
                        "fanotify",
                        Actor::unknown(),
                    ));
                    offset += event_len;
                    continue;
                }
                let mask = u64::from_ne_bytes(buffer[offset + 8..offset + 16].try_into().unwrap());
                let pid = i32::from_ne_bytes(buffer[offset + 20..offset + 24].try_into().unwrap());
                if mask & libc::FAN_Q_OVERFLOW != 0 {
                    let _ = tx.send(event(
                        Action::Overflow,
                        PathBuf::from("/"),
                        true,
                        "fanotify",
                        classify_actor(pid),
                    ));
                    offset += event_len;
                    continue;
                }
                let payload = &buffer[offset..offset + event_len];
                let actor = classify_actor(pid);
                let (target, old, new) = self.parse_info_records(payload);
                let is_dir = mask & libc::FAN_ONDIR != 0;
                let has_move =
                    mask & (libc::FAN_MOVED_FROM | libc::FAN_MOVED_TO | libc::FAN_RENAME) != 0;
                if has_move {
                    match (old, new) {
                        (Some(old), Some(new)) => {
                            let mut moved = event(Action::Move, new, is_dir, "fanotify", actor);
                            moved.old_path = Some(old);
                            let _ = tx.send(moved);
                        }
                        (Some(old), None) => {
                            let _ = tx.send(event(Action::Remove, old, is_dir, "fanotify", actor));
                        }
                        (None, Some(new)) => {
                            let _ =
                                tx.send(event(Action::Reconcile, new, is_dir, "fanotify", actor));
                        }
                        (None, None) => {
                            if let Some(path) = target {
                                let action = if mask & libc::FAN_MOVED_FROM != 0 {
                                    Action::Remove
                                } else if is_dir {
                                    Action::Reconcile
                                } else {
                                    Action::Upsert
                                };
                                let _ = tx.send(event(action, path, is_dir, "fanotify", actor));
                            } else {
                                let _ = tx.send(event(
                                    Action::Overflow,
                                    self.roots
                                        .first()
                                        .map(|root| root.path.clone())
                                        .unwrap_or_else(|| PathBuf::from("/")),
                                    true,
                                    "fanotify",
                                    actor,
                                ));
                            }
                        }
                    }
                } else if let Some(path) = target {
                    let action = if mask & (libc::FAN_DELETE | libc::FAN_DELETE_SELF) != 0 {
                        Action::Remove
                    } else if mask
                        & (libc::FAN_CREATE
                            | libc::FAN_CLOSE_WRITE
                            | libc::FAN_MODIFY
                            | libc::FAN_ATTRIB)
                        != 0
                    {
                        Action::Upsert
                    } else {
                        Action::Reconcile
                    };
                    let mut changed = event(action, path, is_dir, "fanotify", actor);
                    changed.event_kind = if mask & (libc::FAN_DELETE | libc::FAN_DELETE_SELF) != 0 {
                        EVENT_DELETE
                    } else if mask & (libc::FAN_MODIFY | libc::FAN_CLOSE_WRITE) != 0 {
                        EVENT_MODIFY
                    } else if mask & libc::FAN_ATTRIB != 0 {
                        EVENT_ATTRIB
                    } else {
                        EVENT_CREATE
                    };
                    let _ = tx.send(changed);
                } else {
                    let _ = tx.send(event(
                        Action::Overflow,
                        PathBuf::from("/"),
                        true,
                        "fanotify",
                        actor,
                    ));
                }
                offset += event_len;
            }
        }
        for mount in self.mounts {
            unsafe {
                libc::close(mount.fd);
            }
        }
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(target_os = "linux")]
fn transient_watch_path_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ENOTDIR)
    )
}

#[cfg(target_os = "linux")]
fn mount_identity(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.dev())
}

#[cfg(target_os = "linux")]
fn mount_candidates(root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    result.push(root.clone());
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return result;
    };
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let _device = fields.next();
        let Some(raw_mount) = fields.next() else {
            continue;
        };
        let mount = PathBuf::from(unescape_proc_mount_field(raw_mount));
        if !mount.starts_with(&root) || mount == root {
            continue;
        }
        let mount_key = normalize_index_dir(&mount);
        if ["/proc", "/sys", "/dev", "/run"]
            .iter()
            .any(|prefix| mount_key == *prefix || mount_key.starts_with(&format!("{prefix}/")))
        {
            continue;
        }
        result.push(mount);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescing_preserves_reconcile_over_later_upsert() {
        let path = PathBuf::from("/tmp/unearth-watch-test");
        let batch = vec![
            event(Action::Upsert, path.clone(), true, "test", Actor::unknown()),
            event(
                Action::Reconcile,
                path.clone(),
                true,
                "test",
                Actor::unknown(),
            ),
            event(Action::Upsert, path, true, "test", Actor::unknown()),
        ];
        let result = coalesce_batch(batch);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].action, Action::Reconcile);
    }

    #[test]
    fn root_matching_does_not_cross_name_boundaries() {
        assert!(path_in_root("/home", Path::new("/home/user")));
        assert!(!path_in_root("/home", Path::new("/home2/user")));
        assert!(path_in_root("/", Path::new("/etc")));
    }

    #[cfg(unix)]
    #[test]
    fn current_process_has_a_stable_owner_identity() {
        let owner = current_owner();
        assert_eq!(owner.pid, i64::from(std::process::id()));
        assert!(!owner.boot_id.is_empty());
        assert!(owner.starttime > 0);
        assert!(owner_is_alive(
            Some(owner.pid),
            Some(&owner.boot_id),
            Some(owner.starttime)
        ));
    }

    #[test]
    fn periodic_reconciliation_is_opt_in() {
        assert_eq!(periodic_reconcile_interval_from(None), None);
        assert_eq!(periodic_reconcile_interval_from(Some("0")), None);
        assert_eq!(periodic_reconcile_interval_from(Some("invalid")), None);
        assert_eq!(
            periodic_reconcile_interval_from(Some("3600")),
            Some(Duration::from_secs(3600))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn vanished_watch_paths_are_transient() {
        assert!(transient_watch_path_error(
            &std::io::Error::from_raw_os_error(libc::ENOENT)
        ));
        assert!(transient_watch_path_error(
            &std::io::Error::from_raw_os_error(libc::ENOTDIR)
        ));
        assert!(!transient_watch_path_error(
            &std::io::Error::from_raw_os_error(libc::EACCES)
        ));
    }
}
