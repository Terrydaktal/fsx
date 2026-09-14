use super::*;
use crossbeam_channel::{bounded, Receiver, Sender};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rayon::ThreadPoolBuilder;

#[path = "watcher_policy.rs"]
mod watcher_policy;
use watcher_policy::periodic_reconcile_interval;
#[cfg(test)]
use watcher_policy::periodic_reconcile_interval_from;

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
const WATCH_BACKEND_WAIT: Duration = Duration::from_millis(500);
const WATCH_MOUNT_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const WATCH_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const MAX_STARTUP_REPLAY_BATCHES: usize = 1024;
const PENDING_MOVE_TIMEOUT: Duration = Duration::from_secs(2);
const WATCH_RESTART_TIMEOUT: Duration = Duration::from_secs(10);
const LIVE_ID_CACHE_CAPACITY: usize = 32_768;

#[derive(Default)]
struct MetricsCounters {
    raw_events: AtomicU64,
    coalesced_events: AtomicU64,
    batches: AtomicU64,
    upserts: AtomicU64,
    removes: AtomicU64,
    moves: AtomicU64,
    reconciles: AtomicU64,
    overflows: AtomicU64,
    refreshes: AtomicU64,
    refresh_nanos: AtomicU64,
    subtree_scans: AtomicU64,
    subtree_scan_nanos: AtomicU64,
    db_transactions: AtomicU64,
    db_nanos: AtomicU64,
    queue_high_water: AtomicU64,
    batch_high_water: AtomicU64,
    cache_high_water: AtomicU64,
}

#[derive(Clone, Copy, Default)]
struct MetricsSnapshot {
    raw_events: u64,
    coalesced_events: u64,
    batches: u64,
    upserts: u64,
    removes: u64,
    moves: u64,
    reconciles: u64,
    overflows: u64,
    refreshes: u64,
    refresh_nanos: u64,
    subtree_scans: u64,
    subtree_scan_nanos: u64,
    db_transactions: u64,
    db_nanos: u64,
    queue_high_water: u64,
    batch_high_water: u64,
    cache_high_water: u64,
}

impl MetricsCounters {
    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            raw_events: self.raw_events.load(Ordering::Relaxed),
            coalesced_events: self.coalesced_events.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            upserts: self.upserts.load(Ordering::Relaxed),
            removes: self.removes.load(Ordering::Relaxed),
            moves: self.moves.load(Ordering::Relaxed),
            reconciles: self.reconciles.load(Ordering::Relaxed),
            overflows: self.overflows.load(Ordering::Relaxed),
            refreshes: self.refreshes.load(Ordering::Relaxed),
            refresh_nanos: self.refresh_nanos.load(Ordering::Relaxed),
            subtree_scans: self.subtree_scans.load(Ordering::Relaxed),
            subtree_scan_nanos: self.subtree_scan_nanos.load(Ordering::Relaxed),
            db_transactions: self.db_transactions.load(Ordering::Relaxed),
            db_nanos: self.db_nanos.load(Ordering::Relaxed),
            queue_high_water: self.queue_high_water.load(Ordering::Relaxed),
            batch_high_water: self.batch_high_water.load(Ordering::Relaxed),
            cache_high_water: self.cache_high_water.load(Ordering::Relaxed),
        }
    }
}

struct DbCaches {
    dirs: ClockCache<i64>,
    names: ClockCache<i64>,
    actors: ClockCache<CachedActor>,
}

#[derive(Clone, Copy)]
struct CachedActor {
    id: i64,
    last_seen: i64,
}

impl DbCaches {
    fn clear(&mut self) {
        self.dirs.clear();
        self.names.clear();
        self.actors.clear();
    }

    fn insert_actor(&mut self, key: String, actor: CachedActor) {
        self.actors.insert(key, actor);
    }

    fn invalidate_dirs_below(&mut self, path: &str) {
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        self.dirs
            .retain(|key| key != path && !key.starts_with(&prefix));
    }
}

impl Default for DbCaches {
    fn default() -> Self {
        Self {
            dirs: ClockCache::new(LIVE_ID_CACHE_CAPACITY),
            names: ClockCache::new(LIVE_ID_CACHE_CAPACITY),
            actors: ClockCache::new(LIVE_ID_CACHE_CAPACITY),
        }
    }
}

struct ClockCache<V> {
    capacity: usize,
    entries: HashMap<String, (V, bool)>,
    clock: VecDeque<String>,
}

impl<V> ClockCache<V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::with_capacity(capacity),
            clock: VecDeque::with_capacity(capacity),
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.clock.clear();
    }

    fn get(&mut self, key: &str) -> Option<&V> {
        let (value, referenced) = self.entries.get_mut(key)?;
        *referenced = true;
        Some(value)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        let (value, referenced) = self.entries.get_mut(key)?;
        *referenced = true;
        Some(value)
    }

    fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    fn insert(&mut self, key: String, value: V) {
        if let Some((current, referenced)) = self.entries.get_mut(&key) {
            *current = value;
            *referenced = true;
            return;
        }
        while self.entries.len() >= self.capacity.max(1) {
            let Some(candidate) = self.clock.pop_front() else {
                break;
            };
            let referenced = self
                .entries
                .get(&candidate)
                .is_some_and(|(_, referenced)| *referenced);
            if referenced {
                if let Some((_, referenced)) = self.entries.get_mut(&candidate) {
                    *referenced = false;
                }
                self.clock.push_back(candidate);
            } else {
                self.entries.remove(&candidate);
            }
        }
        self.clock.push_back(key.clone());
        self.entries.insert(key, (value, true));
    }

    fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.entries.retain(|key, _| keep(key));
        self.clock.retain(|key| self.entries.contains_key(key));
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Clone, Copy, Default)]
struct ProcessSample {
    rss_bytes: u64,
    vm_bytes: u64,
    threads: u64,
    cpu_user_nanos: u64,
    cpu_system_nanos: u64,
}

#[cfg(target_os = "linux")]
fn read_process_sample() -> ProcessSample {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let value = |key: &str, multiplier: u64| {
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|raw| raw.parse::<u64>().ok())
                    .map(|value| value.saturating_mul(multiplier))
            })
            .unwrap_or(0)
    };
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    let (cpu_user_nanos, cpu_system_nanos) =
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            (
                timeval_to_nanos(usage.ru_utime),
                timeval_to_nanos(usage.ru_stime),
            )
        } else {
            (0, 0)
        };
    ProcessSample {
        rss_bytes: value("VmRSS:", 1024),
        vm_bytes: value("VmSize:", 1024),
        threads: value("Threads:", 1),
        cpu_user_nanos,
        cpu_system_nanos,
    }
}

#[cfg(not(target_os = "linux"))]
fn read_process_sample() -> ProcessSample {
    ProcessSample::default()
}

#[cfg(target_os = "linux")]
fn timeval_to_nanos(value: libc::timeval) -> u64 {
    let seconds = u64::try_from(value.tv_sec).unwrap_or(0);
    let micros = u64::try_from(value.tv_usec).unwrap_or(0);
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(micros.saturating_mul(1_000))
}

struct MetricsLogger {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MetricsLogger {
    fn start(
        path: &Path,
        counters: Arc<MetricsCounters>,
        ttl: Duration,
        max_bytes: u64,
    ) -> Result<Self, String> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "cannot create metrics report directory '{}': {error}",
                    parent.display()
                )
            })?;
        }
        let file = File::create(path).map_err(|error| {
            format!("cannot create metrics report '{}': {error}", path.display())
        })?;
        let mut writer = BufWriter::new(file);
        writeln!(
            writer,
            "timestamp_ms\telapsed_ms\tpid\trss_bytes\tvm_bytes\tthreads\tcpu_user_ms\tcpu_system_ms\tcpu_total_ms\tcpu_percent\traw_events\tcoalesced_events\tbatches\tupserts\tremoves\tmoves\treconciles\toverflows\trefreshes\trefresh_ms\tsubtree_scans\tsubtree_scan_ms\tdb_transactions\tdb_ms\tqueue_high_water\tbatch_high_water\tcache_high_water"
        )
        .map_err(|error| format!("cannot write metrics report header: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("cannot flush metrics report header: {error}"))?;
        let header_bytes = writer
            .get_ref()
            .metadata()
            .map_err(|error| format!("cannot inspect metrics report size: {error}"))?
            .len();
        if max_bytes == 0 || header_bytes > max_bytes {
            return Err("metrics report byte limit is smaller than its header".to_string());
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let pid = std::process::id();
        let started = Instant::now();
        let handle = thread::Builder::new()
            .name("unearth-metrics".to_string())
            .spawn(move || {
                let mut writer = writer;
                let mut previous = None;
                let mut last_written = Instant::now();
                let sample = read_process_sample();
                if let Err(error) = write_metrics_row(
                    &mut writer,
                    started,
                    last_written,
                    previous,
                    sample,
                    pid,
                    &counters,
                ) {
                    eprintln!("unearth: metrics report write failed: {error}");
                    return;
                }
                previous = Some((last_written, sample));
                let _ = writer.flush();

                while !thread_stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(1));
                    if started.elapsed() >= ttl {
                        break;
                    }
                    let now = Instant::now();
                    let sample = read_process_sample();
                    if let Err(error) = write_metrics_row(
                        &mut writer,
                        started,
                        now,
                        previous,
                        sample,
                        pid,
                        &counters,
                    ) {
                        eprintln!("unearth: metrics report write failed: {error}");
                        break;
                    }
                    let _ = writer.flush();
                    if writer
                        .get_ref()
                        .metadata()
                        .map(|metadata| metadata.len() >= max_bytes)
                        .unwrap_or(true)
                    {
                        break;
                    }
                    previous = Some((now, sample));
                    last_written = now;
                }

                let now = Instant::now();
                if now.duration_since(last_written) >= Duration::from_millis(100) {
                    let sample = read_process_sample();
                    if write_metrics_row(
                        &mut writer,
                        started,
                        now,
                        previous,
                        sample,
                        pid,
                        &counters,
                    )
                    .is_ok()
                    {
                        let _ = writer.flush();
                    }
                }
            })
            .map_err(|error| format!("cannot start metrics sampler: {error}"))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for MetricsLogger {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn write_metrics_row(
    writer: &mut BufWriter<File>,
    started: Instant,
    at: Instant,
    previous: Option<(Instant, ProcessSample)>,
    sample: ProcessSample,
    pid: u32,
    counters: &MetricsCounters,
) -> io::Result<()> {
    let elapsed = at.duration_since(started);
    let cpu_total_nanos = sample
        .cpu_user_nanos
        .saturating_add(sample.cpu_system_nanos);
    let cpu_percent = previous
        .and_then(|(previous_at, previous_sample)| {
            let wall_nanos = at.duration_since(previous_at).as_nanos();
            if wall_nanos == 0 {
                return None;
            }
            let previous_cpu = previous_sample
                .cpu_user_nanos
                .saturating_add(previous_sample.cpu_system_nanos);
            Some((cpu_total_nanos.saturating_sub(previous_cpu) as f64 / wall_nanos as f64) * 100.0)
        })
        .unwrap_or(0.0);
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default();
    let metrics = counters.snapshot();
    writeln!(
        writer,
        "{}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{}\t{:.3}\t{}\t{:.3}\t{}\t{}\t{}",
        timestamp_ms,
        elapsed.as_millis(),
        pid,
        sample.rss_bytes,
        sample.vm_bytes,
        sample.threads,
        sample.cpu_user_nanos as f64 / 1_000_000.0,
        sample.cpu_system_nanos as f64 / 1_000_000.0,
        cpu_total_nanos as f64 / 1_000_000.0,
        cpu_percent,
        metrics.raw_events,
        metrics.coalesced_events,
        metrics.batches,
        metrics.upserts,
        metrics.removes,
        metrics.moves,
        metrics.reconciles,
        metrics.overflows,
        metrics.refreshes,
        metrics.refresh_nanos as f64 / 1_000_000.0,
        metrics.subtree_scans,
        metrics.subtree_scan_nanos as f64 / 1_000_000.0,
        metrics.db_transactions,
        metrics.db_nanos as f64 / 1_000_000.0,
        metrics.queue_high_water,
        metrics.batch_high_water,
        metrics.cache_high_water,
    )
}

fn add_elapsed(counter: &AtomicU64, started: Instant) {
    counter.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

fn metric_add(counter: Option<&AtomicU64>, amount: u64) {
    if let Some(counter) = counter {
        counter.fetch_add(amount, Ordering::Relaxed);
    }
}

fn metric_elapsed(counter: Option<&AtomicU64>, started: Instant) {
    if let Some(counter) = counter {
        add_elapsed(counter, started);
    }
}

fn metric_max(counter: Option<&AtomicU64>, value: u64) {
    let Some(counter) = counter else {
        return;
    };
    let mut current = counter.load(Ordering::Relaxed);
    while value > current {
        match counter.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

#[cfg(unix)]
static WATCH_STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_watch_signal(_signal: libc::c_int) {
    WATCH_STOP.store(true, Ordering::Relaxed);
}

#[cfg(unix)]
fn install_watch_signal_handlers() {
    unsafe {
        let handler = handle_watch_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IndexedPathState {
    Missing,
    NonDirectory,
    Directory,
}

mod snapshot_budget;
use snapshot_budget::{SnapshotBudget, SnapshotLease};

const WATCH_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
const WATCH_QUEUED_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

struct ReconcileScan {
    entries: Arc<Vec<ScannedIndexEntry>>,
    _lease: Option<Arc<SnapshotLease>>,
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
    scanned_entries: Option<Arc<Vec<ScannedIndexEntry>>>,
    snapshot_lease: Option<Arc<SnapshotLease>>,
}

#[derive(Clone)]
struct EventSender {
    inner: Sender<FsEvent>,
    pending_modifies: Arc<Mutex<HashSet<PathBuf>>>,
    snapshot_budget: Arc<SnapshotBudget>,
}

struct EventReceiver {
    inner: Receiver<FsEvent>,
    pending_modifies: Arc<Mutex<HashSet<PathBuf>>>,
}

fn event_queue(capacity: usize) -> (EventSender, EventReceiver) {
    let (sender, receiver) = bounded(capacity);
    let pending_modifies = Arc::new(Mutex::new(HashSet::new()));
    (
        EventSender {
            inner: sender,
            pending_modifies: Arc::clone(&pending_modifies),
            snapshot_budget: SnapshotBudget::new(WATCH_QUEUED_SNAPSHOT_BYTES),
        },
        EventReceiver {
            inner: receiver,
            pending_modifies,
        },
    )
}

impl EventSender {
    fn send(&self, mut event: FsEvent) -> Result<(), ()> {
        if let Some(entries) = event.scanned_entries.as_ref() {
            let bytes = entries.iter().fold(
                entries
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ScannedIndexEntry>()),
                |bytes, entry| bytes.saturating_add(entry.path.capacity()),
            );
            event.snapshot_lease = self.snapshot_budget.reserve(bytes);
            if event.snapshot_lease.is_none() {
                // Retain the reconciliation event, not a partial snapshot.
                // Its consumer performs a fresh full subtree reconciliation.
                event.scanned_entries = None;
            }
        }
        let coalescible = event.action == Action::Upsert && event.event_kind == EVENT_MODIFY;
        if coalescible {
            let mut pending = self
                .pending_modifies
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !pending.insert(event.path.clone()) {
                return Ok(());
            }
        } else {
            // Structural and recovery events are ordering barriers. Permit a
            // later modify to enter the queue even when an older modify for
            // the same path has not been consumed yet.
            self.pending_modifies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        let path = coalescible.then(|| event.path.clone());
        match self.inner.send(event) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Some(path) = path {
                    self.pending_modifies
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&path);
                }
                let _ = error;
                Err(())
            }
        }
    }
}

impl EventReceiver {
    fn len(&self) -> usize {
        self.inner.len()
    }

    fn mark_dequeued(&self, event: &FsEvent) {
        if event.action == Action::Upsert && event.event_kind == EVENT_MODIFY {
            self.pending_modifies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&event.path);
        }
    }

    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<FsEvent, crossbeam_channel::RecvTimeoutError> {
        let event = self.inner.recv_timeout(timeout)?;
        self.mark_dequeued(&event);
        Ok(event)
    }

    fn try_recv(&self) -> Result<FsEvent, crossbeam_channel::TryRecvError> {
        let event = self.inner.try_recv()?;
        self.mark_dequeued(&event);
        Ok(event)
    }
}

#[derive(Clone, Debug)]
struct RootState {
    key: String,
    path: PathBuf,
}

type InitialScans = HashMap<String, Vec<ScannedIndexEntry>>;
type StartedBackend = (EventReceiver, String, Option<String>, InitialScans);

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

fn stop_existing_watchers(roots: &[RootState]) -> Result<(), String> {
    let conn = open_index_db_writer()?;
    let current = current_owner();
    let mut owners = Vec::new();
    for root in roots {
        let existing: Option<ExistingOwner> = conn
            .query_row(
                "SELECT watcher_pid, owner_boot_id, owner_starttime, status
                 FROM watch_state WHERE root=?1",
                [&root.key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((Some(pid), Some(boot_id), Some(starttime), status)) = existing else {
            continue;
        };
        let same_owner =
            pid == current.pid && boot_id == current.boot_id && starttime == current.starttime;
        if same_owner
            || !matches!(
                status.as_deref(),
                Some("starting" | "running" | "recovering")
            )
            || !owner_is_alive(Some(pid), Some(&boot_id), Some(starttime))
        {
            continue;
        }
        if !owners
            .iter()
            .any(|(existing_pid, existing_boot, existing_start)| {
                *existing_pid == pid && existing_boot == &boot_id && *existing_start == starttime
            })
        {
            owners.push((pid, boot_id, starttime));
        }
    }
    for (pid, boot_id, starttime) in &owners {
        let mut stmt = conn
            .prepare(
                "SELECT root FROM watch_state
                 WHERE watcher_pid=?1 AND owner_boot_id=?2 AND owner_starttime=?3
                   AND status IN ('starting', 'running', 'recovering')",
            )
            .map_err(|e| e.to_string())?;
        let owned_roots = stmt
            .query_map(params![pid, boot_id, starttime], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        if owned_roots
            .iter()
            .any(|owned| !roots.iter().any(|root| &root.key == owned))
        {
            return Err(format!(
                "watcher pid {pid} owns additional roots; stop it explicitly before restarting"
            ));
        }
    }
    drop(conn);

    for (pid, boot_id, starttime) in &owners {
        eprintln!(
            "unearth: stopping existing watcher pid {} before restarting its root",
            pid
        );
        let result = unsafe { libc::kill(*pid as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("cannot stop existing watcher pid {}: {error}", pid));
            }
        }
        let deadline = Instant::now() + WATCH_RESTART_TIMEOUT;
        while owner_is_alive(Some(*pid), Some(boot_id), Some(*starttime)) {
            if Instant::now() >= deadline {
                return Err(format!(
                    "existing watcher pid {} did not stop within {} seconds",
                    pid,
                    WATCH_RESTART_TIMEOUT.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}

fn claim_watch_states(roots: &[RootState]) -> Result<(), String> {
    stop_existing_watchers(roots)?;
    let owner = current_owner();
    let conn = open_index_db_writer()?;
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
    tx.commit().map_err(|e| e.to_string())?;
    for root in roots {
        record_watch_event(&root.key, "claim", "watcher claimed root");
        update_watch_task(&root.key, "watcher", "starting", None);
    }
    Ok(())
}

fn heartbeat_states(
    conn: &mut Connection,
    roots: &[RootState],
    backend: &str,
    owner: &WatcherOwner,
) -> Result<(), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for root in roots {
        tx.execute(
            "UPDATE watch_state SET backend=?2, heartbeat=?3, watcher_pid=?4,
                 status=CASE WHEN dirty=0 THEN 'running' ELSE status END,
                 online=1, error=CASE WHEN dirty=0 THEN NULL ELSE error END
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
    if let Ok(conn) = open_index_db_writer() {
        for root in roots {
            let _ = conn.execute(
                "UPDATE watch_state SET status='stopped', online=0, dirty=0
                 WHERE root=?1 AND owner_boot_id=?2 AND owner_starttime=?3",
                params![root.key, owner.boot_id, owner.starttime],
            );
            record_watch_event(&root.key, "shutdown", "watcher stopped");
            update_watch_task(&root.key, "watcher", "stopped", None);
            update_watch_task(&root.key, "database", "stopped", None);
            update_watch_task(&root.key, "query-server", "stopped", None);
            update_watch_task(&root.key, "metrics", "stopped", None);
        }
    }
}

struct WatchStateGuard {
    roots: Vec<RootState>,
}

impl Drop for WatchStateGuard {
    fn drop(&mut self) {
        shutdown_states(&self.roots);
    }
}

#[allow(dead_code)]
pub(crate) fn print_status() -> Result<(), String> {
    let _ = initialize_index_db()?;
    let conn = open_index_db_writer()?;
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

#[allow(dead_code)]
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
    let _ = initialize_index_db()?;
    let mut roots = Vec::new();
    for (index, raw) in opts.positional.iter().enumerate() {
        let raw_path = (opts.positional_os.len() == opts.positional.len())
            .then(|| PathBuf::from(&opts.positional_os[index]));
        let path =
            fs::canonicalize(raw_path.unwrap_or_else(|| PathBuf::from(expand_home_path(raw))))
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
    // Overlapping roots make one event belong to multiple ownership domains
    // and cause duplicate subtree scans. Require callers to watch the common
    // ancestor once instead.
    let mut root_keys: Vec<&str> = roots.iter().map(|root| root.key.as_str()).collect();
    root_keys.sort_unstable();
    for pair in root_keys.windows(2) {
        if let [ancestor, child] = pair {
            let prefix = if *ancestor == "/" {
                "/".to_string()
            } else {
                format!("{ancestor}/")
            };
            if !child.starts_with(&prefix) {
                continue;
            }
            return Err(format!(
                "watch roots overlap: '{}' contains '{}'",
                ancestor, child
            ));
        }
    }
    let worker_threads = if !opts.threads_explicit
        && roots
            .iter()
            .all(|root| root_prefers_single_thread(&root.path))
    {
        1
    } else {
        opts.threads_override.max(1)
    };
    let _ = ThreadPoolBuilder::new()
        .num_threads(worker_threads)
        .build_global();

    #[cfg(unix)]
    {
        WATCH_STOP.store(false, Ordering::Relaxed);
        install_watch_signal_handlers();
    }

    claim_watch_states(&roots)?;
    for root in &roots {
        record_watch_config(&root.key, opts);
    }
    let _watch_state_guard = WatchStateGuard {
        roots: roots.clone(),
    };
    let _query_server = start_query_server()?;
    for root in &roots {
        update_watch_task(&root.key, "query-server", "running", None);
        update_watch_task(&root.key, "database", "starting", None);
    }
    let mut metrics_counters = None;
    let diagnostics_disabled = std::env::var("FSX_DIAGNOSTICS")
        .map(|value| matches!(value.as_str(), "0" | "off" | "false"))
        .unwrap_or(false);
    let _metrics_logger = if !diagnostics_disabled {
        if let Some(path) = opts
            .watch_metrics_os
            .as_deref()
            .map(Path::new)
            .or_else(|| opts.watch_metrics.as_deref().map(Path::new))
        {
            let counters = Arc::new(MetricsCounters::default());
            let logger = match MetricsLogger::start(
                path,
                Arc::clone(&counters),
                Duration::from_secs(opts.watch_metrics_ttl.max(1)),
                opts.watch_metrics_max_bytes,
            ) {
                Ok(logger) => logger,
                Err(error) => {
                    return Err(error);
                }
            };
            metrics_counters = Some(counters);
            for root in &roots {
                update_watch_task(&root.key, "metrics", "running", None);
            }
            Some(logger)
        } else {
            None
        }
    } else {
        eprintln!("unearth: optional diagnostics disabled by FSX_DIAGNOSTICS");
        None
    };
    let (events, backend, backend_error, mut initial_scans) = match start_backend(&roots, opts) {
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
    let mut event_conn = open_index_db_writer()?;
    for root in &roots {
        update_watch_task(&root.key, "database", "running", None);
    }
    let owner = current_owner();
    let mut db_caches = DbCaches::default();

    let mut initial_error = None;
    for root in &roots {
        if WATCH_STOP.load(Ordering::Relaxed) {
            return Err("watcher interrupted during initial index scan".to_string());
        }
        let started = Instant::now();
        let result = if let Some(entries) = initial_scans.remove(&root.key) {
            if WATCH_STOP.load(Ordering::Relaxed) {
                Err("watcher interrupted during initial index scan".to_string())
            } else {
                refresh_index_root_from_scan(&root.key, entries)
            }
        } else {
            refresh_index_root_cancellable(&root.key, opts, Some(&WATCH_STOP))
        };
        metric_add(
            metrics_counters
                .as_deref()
                .map(|metrics| &metrics.refreshes),
            1,
        );
        metric_elapsed(
            metrics_counters
                .as_deref()
                .map(|metrics| &metrics.refresh_nanos),
            started,
        );
        if let Err(error) = result {
            initial_error = Some(format!("{}: {}", root.key, error));
            update_state_error(&root.key, &backend, &error)?;
        } else {
            mark_initial_reconciled(&root.key, &backend)?;
        }
    }
    if let Some(error) = initial_error {
        return Err(format!("initial live index scan failed: {}", error));
    }
    if WATCH_STOP.load(Ordering::Relaxed) {
        return Err("watcher interrupted during initial index scan".to_string());
    }
    let mut replay_batches = 0usize;
    while let Ok(first) = events.recv_timeout(Duration::from_millis(10)) {
        replay_batches = replay_batches.saturating_add(1);
        let mut startup_events = vec![first];
        while startup_events.len() < WATCH_BATCH_MAX {
            match events.try_recv() {
                Ok(event) => startup_events.push(event),
                Err(crossbeam_channel::TryRecvError::Empty)
                | Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
        metric_max(
            metrics_counters
                .as_deref()
                .map(|metrics| &metrics.queue_high_water),
            events.len() as u64,
        );
        metric_max(
            metrics_counters
                .as_deref()
                .map(|metrics| &metrics.batch_high_water),
            startup_events.len() as u64,
        );
        if let Err(error) = process_batch_reliably(
            &roots,
            &backend,
            opts,
            startup_events,
            &mut event_conn,
            &mut db_caches,
            metrics_counters.as_deref(),
        ) {
            for root in &roots {
                let _ = update_state_error(&root.key, &backend, &error);
            }
            return Err(format!("initial live event replay failed: {error}"));
        }
        if replay_batches >= MAX_STARTUP_REPLAY_BATCHES {
            eprintln!(
                "unearth: startup event replay reached its bound; continuing with live processing"
            );
            break;
        }
    }
    if WATCH_STOP.load(Ordering::Relaxed) {
        return Err("watcher interrupted during initial event replay".to_string());
    }
    drop(initial_scans);
    purge_unused_allocator_pages();
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
        if last_heartbeat.elapsed() >= WATCH_HEARTBEAT_INTERVAL {
            heartbeat_states(&mut event_conn, &roots, &backend, &owner)?;
            last_heartbeat = std::time::Instant::now();
        }
        if reconcile_interval.is_some_and(|interval| last_periodic_reconcile.elapsed() >= interval)
        {
            for root in &roots {
                let started = Instant::now();
                let result = reconcile_subtree(&mut event_conn, &root.path, opts, &mut db_caches);
                metric_add(
                    metrics_counters
                        .as_deref()
                        .map(|metrics| &metrics.subtree_scans),
                    1,
                );
                metric_elapsed(
                    metrics_counters
                        .as_deref()
                        .map(|metrics| &metrics.subtree_scan_nanos),
                    started,
                );
                if let Err(error) = result {
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
                let error = "live watcher backend stopped".to_string();
                for root in &roots {
                    let _ = update_state_error(&root.key, &backend, &error);
                }
                return Err(error);
            }
        };
        let mut batch = vec![first];
        let deadline = std::time::Instant::now() + WATCH_BATCH_DELAY;
        while batch.len() < WATCH_BATCH_MAX {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match events.recv_timeout(remaining) {
                Ok(event) => batch.push(event),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
                | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        metric_max(
            metrics_counters
                .as_deref()
                .map(|metrics| &metrics.queue_high_water),
            events.len() as u64,
        );
        metric_max(
            metrics_counters
                .as_deref()
                .map(|metrics| &metrics.batch_high_water),
            batch.len() as u64,
        );
        if let Err(error) = process_batch_reliably(
            &roots,
            &backend,
            opts,
            batch,
            &mut event_conn,
            &mut db_caches,
            metrics_counters.as_deref(),
        ) {
            eprintln!("unearth: live event recovery failed: {error}; stopping watcher");
            for root in &roots {
                let _ = update_state_error(&root.key, &backend, &error);
            }
            return Err(error);
        }
    }
    Ok(())
}

fn process_batch_reliably(
    roots: &[RootState],
    backend: &str,
    opts: &Options,
    batch: Vec<FsEvent>,
    conn: &mut Connection,
    db_caches: &mut DbCaches,
    metrics: Option<&MetricsCounters>,
) -> Result<(), String> {
    let global_overflow = batch
        .iter()
        .any(|event| event.action == Action::Overflow && event.path == Path::new("/"));
    let affected_roots = roots
        .iter()
        .filter(|root| {
            global_overflow
                || batch.iter().any(|event| {
                    path_in_root(&root.key, &event.path)
                        || event
                            .old_path
                            .as_deref()
                            .is_some_and(|path| path_in_root(&root.key, path))
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    if global_overflow {
        for root in &affected_roots {
            record_watch_event(
                &root.key,
                "overflow",
                "watch queue overflow; reconciliation required",
            );
        }
    }
    match process_batch(roots, backend, opts, batch, conn, db_caches, metrics) {
        Ok(()) => Ok(()),
        Err(batch_error) => {
            // A complete scan is the source of truth after any partial commit,
            // database error, unresolved event, or failed subtree reconciliation.
            let mut recovery_errors = Vec::new();
            for root in &affected_roots {
                let _ = update_state_error(&root.key, backend, &batch_error);
                db_caches.clear();
                let started = Instant::now();
                let result = refresh_index_root_cancellable(&root.key, opts, Some(&WATCH_STOP));
                metric_add(metrics.map(|metrics| &metrics.refreshes), 1);
                metric_elapsed(metrics.map(|metrics| &metrics.refresh_nanos), started);
                db_caches.clear();
                match result {
                    Ok(()) => {
                        record_watch_event(&root.key, "recovery", "full reconciliation completed");
                        if let Err(error) = mark_reconciled(&root.key, backend) {
                            recovery_errors.push(format!("{}: {error}", root.key));
                        }
                    }
                    Err(error) => {
                        record_watch_event(&root.key, "recovery-error", &error);
                        let _ = update_state_error(&root.key, backend, &error);
                        recovery_errors.push(format!("{}: {error}", root.key));
                    }
                }
            }
            if recovery_errors.is_empty() {
                eprintln!(
                    "unearth: recovered failed live event batch with a full index refresh: {batch_error}"
                );
                Ok(())
            } else {
                Err(format!(
                    "{batch_error}; full recovery failed for {}",
                    recovery_errors.join("; ")
                ))
            }
        }
    }
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
    let conn = open_index_db_writer()?;
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
    tx.commit().map_err(|e| e.to_string())?;
    for root in roots {
        let detail = error.unwrap_or(status);
        record_watch_event(&root.key, "state", detail);
        update_watch_task(&root.key, "watcher", status, error);
    }
    Ok(())
}

fn update_state_error(root: &str, backend: &str, error: &str) -> Result<(), String> {
    let owner = current_owner();
    let conn = open_index_db_writer()?;
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
    record_watch_event(root, "error", error);
    update_watch_task(root, "watcher", "error", Some(error));
    Ok(())
}

fn mark_reconciled(root: &str, backend: &str) -> Result<(), String> {
    mark_reconciled_status(root, backend, "running")
}

fn mark_initial_reconciled(root: &str, backend: &str) -> Result<(), String> {
    mark_reconciled_status(root, backend, "starting")
}

fn mark_reconciled_status(root: &str, backend: &str, status: &str) -> Result<(), String> {
    let owner = current_owner();
    let conn = open_index_db_writer()?;
    conn.execute(
        "INSERT INTO watch_state(root, backend, status, generation, last_reconcile,
                                 dirty, online, watcher_pid, error,
                                 owner_boot_id, owner_starttime, heartbeat)
         VALUES (?1, ?2, ?3, 0, ?4, 0, 1, ?5, NULL, ?6, ?7, ?4)
         ON CONFLICT(root) DO UPDATE SET backend=excluded.backend, status=excluded.status,
             last_reconcile=excluded.last_reconcile, dirty=0, online=1,
             watcher_pid=excluded.watcher_pid, error=NULL,
             owner_boot_id=excluded.owner_boot_id, owner_starttime=excluded.owner_starttime,
             heartbeat=excluded.heartbeat",
        params![
            root,
            backend,
            status,
            now_seconds(),
            owner.pid,
            owner.boot_id,
            owner.starttime
        ],
    )
    .map_err(|e| e.to_string())?;
    record_watch_event(root, "reconcile", "root reconciled");
    update_watch_task(root, "watcher", status, None);
    Ok(())
}

fn process_batch(
    roots: &[RootState],
    backend: &str,
    opts: &Options,
    batch: Vec<FsEvent>,
    conn: &mut Connection,
    db_caches: &mut DbCaches,
    metrics: Option<&MetricsCounters>,
) -> Result<(), String> {
    metric_max(
        metrics.map(|metrics| &metrics.batch_high_water),
        batch.len() as u64,
    );
    metric_add(
        metrics.map(|metrics| &metrics.raw_events),
        batch.len() as u64,
    );
    let batch = coalesce_batch(batch);
    metric_add(
        metrics.map(|metrics| &metrics.coalesced_events),
        batch.len() as u64,
    );
    metric_add(metrics.map(|metrics| &metrics.batches), 1);
    let mut full_refresh = HashSet::<String>::new();
    let mut reconcile = HashSet::<PathBuf>::new();
    let mut reconcile_scans = HashMap::<PathBuf, ReconcileScan>::new();
    let mut recovery_roots = HashSet::<String>::new();
    let mut touched_roots = HashMap::<String, i64>::new();
    let mut touched_parents = HashSet::<PathBuf>::new();
    let db_started = Instant::now();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let now = now_seconds();
    for event in &batch {
        let new_root = matching_root(roots, &event.path);
        let old_root = event
            .old_path
            .as_deref()
            .and_then(|path| matching_root(roots, path));
        if new_root.is_none() && old_root.is_none() && event.action == Action::Overflow {
            for root in roots {
                full_refresh.insert(root.key.clone());
                recovery_roots.insert(root.key.clone());
                tx.execute(
                    "UPDATE watch_state SET dirty=1, status='recovering', last_event=?2,
                     online=1 WHERE root=?1",
                    params![root.key, now],
                )
                .map_err(|e| e.to_string())?;
                record_state_touch(&mut touched_roots, &root.key, event.at);
            }
            continue;
        }
        if event.action == Action::Move {
            metric_add(metrics.map(|metrics| &metrics.moves), 1);
            let mut changed_roots = HashSet::new();
            let old_key = event.old_path.as_deref().map(normalize_index_dir);
            let new_key = normalize_index_dir(&event.path);
            let old_is_indexed = old_root
                .zip(old_key.as_deref())
                .is_some_and(|(root, path)| !is_root_index_excluded_path(&root.key, path));
            let new_is_indexed =
                new_root.is_some_and(|root| !is_root_index_excluded_path(&root.key, &new_key));
            if event.is_dir
                && old_is_indexed
                && new_is_indexed
                && move_indexed_directory(
                    &tx,
                    event.old_path.as_deref().unwrap(),
                    &event.path,
                    &event.actor,
                    event.event_kind,
                    db_caches,
                )?
            {
                for (root, path) in [
                    (old_root.unwrap(), event.old_path.as_deref().unwrap()),
                    (new_root.unwrap(), event.path.as_path()),
                ] {
                    upsert_parent_directory_once(
                        &tx,
                        root,
                        path,
                        &event.actor,
                        event.event_kind,
                        db_caches,
                        &mut touched_parents,
                    )?;
                    changed_roots.insert(root.key.clone());
                }
                for root in changed_roots {
                    record_state_touch(&mut touched_roots, &root, event.at);
                }
                continue;
            }
            if let (Some(old_path), Some(root)) = (event.old_path.as_deref(), old_root) {
                if old_is_indexed {
                    remove_path(&tx, old_path, event.is_dir, db_caches)?;
                    upsert_parent_directory_once(
                        &tx,
                        root,
                        old_path,
                        &event.actor,
                        event.event_kind,
                        db_caches,
                        &mut touched_parents,
                    )?;
                    changed_roots.insert(root.key.clone());
                }
            }
            if let Some(root) = new_root {
                if new_is_indexed {
                    let state = upsert_path(
                        &tx,
                        &event.path,
                        event.is_dir,
                        &event.actor,
                        event.event_kind,
                        db_caches,
                    )?;
                    if state == IndexedPathState::Directory {
                        reconcile.insert(event.path.clone());
                        if let Some(entries) = event.scanned_entries.as_ref() {
                            reconcile_scans.insert(
                                event.path.clone(),
                                ReconcileScan {
                                    entries: Arc::clone(entries),
                                    _lease: event.snapshot_lease.clone(),
                                },
                            );
                        }
                    }
                    upsert_parent_directory_once(
                        &tx,
                        root,
                        &event.path,
                        &event.actor,
                        event.event_kind,
                        db_caches,
                        &mut touched_parents,
                    )?;
                    changed_roots.insert(root.key.clone());
                }
            }
            for root in changed_roots {
                record_state_touch(&mut touched_roots, &root, event.at);
            }
            continue;
        }
        let Some(root) = new_root else {
            continue;
        };
        if is_root_index_excluded_path(&root.key, &normalize_index_dir(&event.path)) {
            continue;
        }
        match event.action {
            Action::Overflow => {
                metric_add(metrics.map(|metrics| &metrics.overflows), 1);
                recovery_roots.insert(root.key.clone());
                if event.path != Path::new("/") && event.path != root.path {
                    if fs::symlink_metadata(&event.path).is_ok() {
                        match upsert_path(
                            &tx,
                            &event.path,
                            event.is_dir,
                            &event.actor,
                            EVENT_RECONCILE,
                            db_caches,
                        )? {
                            IndexedPathState::Directory => {
                                reconcile.insert(event.path.clone());
                            }
                            IndexedPathState::Missing | IndexedPathState::NonDirectory => {}
                        }
                    }
                    upsert_parent_directory_once(
                        &tx,
                        root,
                        &event.path,
                        &event.actor,
                        EVENT_RECONCILE,
                        db_caches,
                        &mut touched_parents,
                    )?;
                } else {
                    full_refresh.insert(root.key.clone());
                }
                tx.execute(
                    "UPDATE watch_state SET dirty=1, status='recovering', last_event=?2,
                     online=1 WHERE root=?1",
                    params![root.key, now],
                )
                .map_err(|e| e.to_string())?;
                record_state_touch(&mut touched_roots, &root.key, event.at);
            }
            Action::Reconcile => {
                metric_add(metrics.map(|metrics| &metrics.reconciles), 1);
                recovery_roots.insert(root.key.clone());
                tx.execute(
                    "UPDATE watch_state SET dirty=1, status='recovering', last_event=?2,
                     online=1 WHERE root=?1",
                    params![root.key, now],
                )
                .map_err(|e| e.to_string())?;
                let state = if event.path == root.path {
                    match fs::symlink_metadata(&event.path) {
                        Ok(metadata) if metadata.file_type().is_dir() => {
                            IndexedPathState::Directory
                        }
                        Ok(_) => IndexedPathState::NonDirectory,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            IndexedPathState::Missing
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                } else {
                    upsert_path(
                        &tx,
                        &event.path,
                        event.is_dir,
                        &event.actor,
                        EVENT_RECONCILE,
                        db_caches,
                    )?
                };
                if state == IndexedPathState::Directory {
                    reconcile.insert(event.path.clone());
                    if let Some(entries) = event.scanned_entries.as_ref() {
                        reconcile_scans.insert(
                            event.path.clone(),
                            ReconcileScan {
                                entries: Arc::clone(entries),
                                _lease: event.snapshot_lease.clone(),
                            },
                        );
                    }
                } else if event.path == root.path {
                    full_refresh.insert(root.key.clone());
                    recovery_roots.insert(root.key.clone());
                }
                if event.path != root.path {
                    upsert_parent_directory_once(
                        &tx,
                        root,
                        &event.path,
                        &event.actor,
                        EVENT_RECONCILE,
                        db_caches,
                        &mut touched_parents,
                    )?;
                }
                record_state_touch(&mut touched_roots, &root.key, event.at);
            }
            Action::Upsert => {
                metric_add(metrics.map(|metrics| &metrics.upserts), 1);
                let _ = upsert_path(
                    &tx,
                    &event.path,
                    event.is_dir,
                    &event.actor,
                    event.event_kind,
                    db_caches,
                )?;
                if event.event_kind == EVENT_CREATE {
                    upsert_parent_directory_once(
                        &tx,
                        root,
                        &event.path,
                        &event.actor,
                        event.event_kind,
                        db_caches,
                        &mut touched_parents,
                    )?;
                }
                record_state_touch(&mut touched_roots, &root.key, event.at);
            }
            Action::Remove => {
                metric_add(metrics.map(|metrics| &metrics.removes), 1);
                remove_path(&tx, &event.path, event.is_dir, db_caches)?;
                upsert_parent_directory_once(
                    &tx,
                    root,
                    &event.path,
                    &event.actor,
                    event.event_kind,
                    db_caches,
                    &mut touched_parents,
                )?;
                record_state_touch(&mut touched_roots, &root.key, event.at);
            }
            Action::Move => unreachable!(),
        }
    }
    touch_states(&tx, touched_roots)?;
    tx.commit().map_err(|e| e.to_string())?;
    metric_add(metrics.map(|metrics| &metrics.db_transactions), 1);
    metric_elapsed(metrics.map(|metrics| &metrics.db_nanos), db_started);
    invalidate_stale_reconcile_scans(&batch, &mut reconcile_scans);
    drop(batch);
    reconcile.retain(|path| !full_refresh.iter().any(|root| path_in_root(root, path)));
    for path in collapse_reconcile_paths(reconcile) {
        let started = Instant::now();
        let result = if let Some(scan) = reconcile_scans.remove(&path) {
            let entries = match Arc::try_unwrap(scan.entries) {
                Ok(entries) => entries,
                Err(entries) => entries.as_ref().clone(),
            };
            reconcile_subtree_from_entries(conn, &path, entries, db_caches)
        } else {
            reconcile_subtree(conn, &path, opts, db_caches)
        };
        metric_add(metrics.map(|metrics| &metrics.subtree_scans), 1);
        metric_elapsed(metrics.map(|metrics| &metrics.subtree_scan_nanos), started);
        if let Err(error) = result {
            if let Some(root) = matching_root(roots, &path) {
                update_state_error(&root.key, backend, &error)?;
                recovery_roots.insert(root.key.clone());
                full_refresh.insert(root.key.clone());
            }
        }
    }
    let mut failed_roots = Vec::new();
    for root in full_refresh {
        db_caches.clear();
        let started = Instant::now();
        let result = refresh_index_root_cancellable(&root, opts, Some(&WATCH_STOP));
        db_caches.clear();
        metric_add(metrics.map(|metrics| &metrics.refreshes), 1);
        metric_elapsed(metrics.map(|metrics| &metrics.refresh_nanos), started);
        if let Err(error) = result {
            update_state_error(&root, backend, &error)?;
            failed_roots.push((root, error));
        } else {
            mark_reconciled(&root, backend)?;
        }
    }
    for root in recovery_roots {
        if !failed_roots.iter().any(|(failed, _)| failed == &root) {
            mark_reconciled(&root, backend)?;
        }
    }
    metric_max(
        metrics.map(|metrics| &metrics.cache_high_water),
        db_caches.dirs.len() as u64 + db_caches.names.len() as u64 + db_caches.actors.len() as u64,
    );
    if !failed_roots.is_empty() {
        return Err(format!(
            "watch recovery failed for {}",
            failed_roots
                .into_iter()
                .map(|(root, error)| format!("{root}: {error}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Ok(())
}

fn invalidate_stale_reconcile_scans(
    batch: &[FsEvent],
    reconcile_scans: &mut HashMap<PathBuf, ReconcileScan>,
) {
    let mut changed_ancestors = HashSet::new();
    for event in batch {
        for path in std::iter::once(&event.path).chain(event.old_path.as_ref()) {
            let key = normalize_index_dir(path);
            for parent in Path::new(&key).ancestors().skip(1) {
                changed_ancestors.insert(parent.to_path_buf());
            }
        }
    }
    reconcile_scans
        .retain(|path, _| !changed_ancestors.contains(Path::new(&normalize_index_dir(path))));
}

fn coalesce_batch(batch: Vec<FsEvent>) -> Vec<FsEvent> {
    let mut result = Vec::with_capacity(batch.len());
    let mut positions = HashMap::<PathBuf, usize>::new();
    for event in batch {
        if matches!(event.action, Action::Move) {
            // A rename is an ordering barrier because it changes two paths.
            // Coalescing later events across it can resurrect the old name or
            // discard an update to the new name.
            positions.clear();
            result.push(event);
            continue;
        }
        let path = event.path.clone();
        if let Some(index) = positions.get(&path).copied() {
            let previous = &result[index];
            let replace = match (previous.action, event.action) {
                (Action::Overflow, _) => false,
                (_, Action::Overflow) => true,
                (Action::Reconcile, Action::Upsert) if previous.scanned_entries.is_some() => true,
                (Action::Reconcile, Action::Upsert) => false,
                (_, Action::Reconcile) => true,
                _ => true,
            };
            if replace {
                if previous.action == Action::Reconcile
                    && previous.scanned_entries.is_some()
                    && event.action == Action::Upsert
                {
                    let mut fallback = event;
                    fallback.action = Action::Reconcile;
                    fallback.event_kind = EVENT_RECONCILE;
                    result[index] = fallback;
                } else {
                    result[index] = event;
                }
            }
        } else {
            positions.insert(path, result.len());
            result.push(event);
        }
    }
    result
}

fn collapse_reconcile_paths(paths: HashSet<PathBuf>) -> Vec<PathBuf> {
    let requests = fsx::hierarchy::PathRequests::new(paths);
    let mut collapsed: Vec<_> = requests
        .roots()
        .map(|(_, path)| path.to_path_buf())
        .collect();
    collapsed.sort_by_cached_key(|path| path.components().count());
    collapsed
}

fn record_state_touch(touched: &mut HashMap<String, i64>, root: &str, at: i64) {
    touched
        .entry(root.to_string())
        .and_modify(|latest| *latest = (*latest).max(at))
        .or_insert(at);
}

fn touch_states(tx: &Transaction<'_>, touched: HashMap<String, i64>) -> Result<(), String> {
    let mut statement = tx
        .prepare_cached(
            "UPDATE watch_state SET last_event=?2, generation=generation+1,
         online=1 WHERE root=?1",
        )
        .map_err(|e| e.to_string())?;
    for (root, at) in touched {
        statement
            .execute(params![root, at / 1_000_000_000])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn actor_id_cached(
    tx: &Transaction<'_>,
    actor: &Actor,
    at: i64,
    caches: &mut DbCaches,
) -> Result<i64, String> {
    let key = actor.executable.clone();
    let timestamp = at / 1_000_000_000;
    if let Some(cached) = caches.actors.get_mut(&key) {
        if cached.last_seen != timestamp {
            tx.prepare_cached("UPDATE actors SET classification=?2, last_seen=?3 WHERE id=?1")
                .map_err(|e| e.to_string())?
                .execute(params![cached.id, actor.classification, timestamp])
                .map_err(|e| e.to_string())?;
            cached.last_seen = timestamp;
        }
        return Ok(cached.id);
    }
    tx.prepare_cached(
        "INSERT INTO actors(executable, classification, first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(executable) DO UPDATE SET classification=excluded.classification,
             last_seen=excluded.last_seen",
    )
    .map_err(|e| e.to_string())?
    .execute(params![key, actor.classification, timestamp])
    .map_err(|e| e.to_string())?;
    let id = tx
        .prepare_cached("SELECT id FROM actors WHERE executable=?1")
        .map_err(|e| e.to_string())?
        .query_row([&actor.executable], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    caches.insert_actor(
        actor.executable.clone(),
        CachedActor {
            id,
            last_seen: timestamp,
        },
    );
    Ok(id)
}

fn ensure_dir_cached(
    tx: &Transaction<'_>,
    path: &str,
    caches: &mut DbCaches,
) -> Result<i64, String> {
    if let Some(id) = caches.dirs.get(path).copied() {
        return Ok(id);
    }
    tx.prepare_cached("INSERT OR IGNORE INTO dirs(path) VALUES (?1)")
        .map_err(|e| e.to_string())?
        .execute([path])
        .map_err(|e| e.to_string())?;
    let id = tx
        .prepare_cached("SELECT id FROM dirs WHERE path=?1")
        .map_err(|e| e.to_string())?
        .query_row([path], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    caches.dirs.insert(path.to_string(), id);
    Ok(id)
}

fn ensure_name_cached(
    tx: &Transaction<'_>,
    name: &str,
    caches: &mut DbCaches,
) -> Result<i64, String> {
    if let Some(id) = caches.names.get(name).copied() {
        return Ok(id);
    }
    tx.prepare_cached("INSERT OR IGNORE INTO strings(value) VALUES (?1)")
        .map_err(|e| e.to_string())?
        .execute([name])
        .map_err(|e| e.to_string())?;
    let id = tx
        .prepare_cached("SELECT id FROM strings WHERE value=?1")
        .map_err(|e| e.to_string())?
        .query_row([name], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    caches.names.insert(name.to_string(), id);
    Ok(id)
}

fn upsert_path(
    tx: &Transaction<'_>,
    raw_path: &Path,
    is_dir_hint: bool,
    actor: &Actor,
    event_kind: i64,
    caches: &mut DbCaches,
) -> Result<IndexedPathState, String> {
    let path = normalize_index_dir(raw_path);
    let metadata = match fs::symlink_metadata(raw_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_path(tx, raw_path, is_dir_hint, caches)?;
            return Ok(IndexedPathState::Missing);
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
    let (parent_key, name) = path
        .rsplit_once('/')
        .map(|(parent, name)| (if parent.is_empty() { "/" } else { parent }, name))
        .ok_or_else(|| "event path has no parent".to_string())?;
    let dir_id = ensure_dir_chain_encoded(tx, parent_key, caches)?;
    let name_id = ensure_name_cached(tx, name, caches)?;
    let actor_id = actor_id_cached(tx, actor, now_nanos(), caches)?;
    let link_count = i64::try_from(metadata.nlink()).ok();
    let (device, inode) = if !is_dir && metadata.nlink() > 1 {
        (
            i64::try_from(metadata.dev()).ok(),
            i64::try_from(metadata.ino()).ok(),
        )
    } else {
        (None, None)
    };
    tx.prepare_cached("DELETE FROM entries WHERE dir_id=?1 AND name_id=?2 AND kind<>?3")
        .map_err(|e| e.to_string())?
        .execute(params![dir_id, name_id, kind])
        .map_err(|e| e.to_string())?;
    tx.prepare_cached(
        "INSERT INTO entries(dir_id, name_id, kind, mtime, size, allocated_size, activity,
                             device, inode, link_count, event_kind, actor_id, actor_uid,
                             actor_pid, event_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(dir_id, name_id, kind) DO UPDATE SET mtime=excluded.mtime,
             size=excluded.size, allocated_size=excluded.allocated_size,
             activity=excluded.activity, device=excluded.device, inode=excluded.inode,
             link_count=excluded.link_count, event_kind=excluded.event_kind,
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
    )
    .map_err(|e| e.to_string())?
    .execute(params![
            dir_id,
            name_id,
            kind,
            metadata_mtime_nanos(&metadata),
            metadata_size_i64(&metadata),
            i64::try_from(::fsx::metadata::allocated_size(&metadata)).ok(),
            metadata_activity_nanos(&metadata),
            device,
            inode,
            link_count,
            event_kind,
            actor_id,
            actor.uid,
            actor.pid,
            now_nanos(),
        ])
    .map_err(|e| e.to_string())?;
    if is_dir {
        ensure_dir_cached(tx, &path, caches)?;
    }
    Ok(if is_dir {
        IndexedPathState::Directory
    } else {
        IndexedPathState::NonDirectory
    })
}

fn upsert_parent_directory(
    tx: &Transaction<'_>,
    root: &RootState,
    path: &Path,
    actor: &Actor,
    event_kind: i64,
    caches: &mut DbCaches,
) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent == root.path || !path_in_root(&root.key, parent) {
        return Ok(());
    }
    let parent_key = normalize_index_dir(parent);
    if is_root_index_excluded_path(&root.key, &parent_key) {
        return Ok(());
    }
    let _ = upsert_path(tx, parent, true, actor, event_kind, caches)?;
    Ok(())
}

fn upsert_parent_directory_once(
    tx: &Transaction<'_>,
    root: &RootState,
    path: &Path,
    actor: &Actor,
    event_kind: i64,
    caches: &mut DbCaches,
    touched: &mut HashSet<PathBuf>,
) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if !touched.insert(parent.to_path_buf()) {
        return Ok(());
    }
    upsert_parent_directory(tx, root, path, actor, event_kind, caches)
}

fn ensure_dir_chain_encoded(
    tx: &Transaction<'_>,
    key: &str,
    caches: &mut DbCaches,
) -> Result<i64, String> {
    if let Some(id) = caches.dirs.get(key).copied() {
        return Ok(id);
    }
    ensure_dir_cached(tx, "/", caches)?;
    if key == "/" {
        return ensure_dir_cached(tx, key, caches);
    }
    let mut current = String::from("/");
    for component in key.trim_start_matches('/').split('/') {
        if !current.ends_with('/') {
            current.push('/');
        }
        current.push_str(component);
        ensure_dir_cached(tx, &current, caches)?;
    }
    ensure_dir_cached(tx, key, caches)
}

fn remove_path(
    tx: &Transaction<'_>,
    raw_path: &Path,
    is_dir_hint: bool,
    caches: &mut DbCaches,
) -> Result<(), String> {
    let path = normalize_index_dir(raw_path);
    if path == "/" {
        return Ok(());
    }
    let (parent_key, name) = path
        .rsplit_once('/')
        .map(|(parent, name)| (if parent.is_empty() { "/" } else { parent }, name))
        .ok_or_else(|| "removed path has no parent".to_string())?;
    let name_id = if let Some(id) = caches.names.get(name).copied() {
        Some(id)
    } else {
        let id: Option<i64> = tx
            .prepare_cached("SELECT id FROM strings WHERE value=?1")
            .map_err(|e| e.to_string())?
            .query_row([name], |row| row.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(id) = id {
            caches.names.insert(name.to_string(), id);
        }
        id
    };
    let parent_id = if let Some(id) = caches.dirs.get(parent_key).copied() {
        Some(id)
    } else {
        let id: Option<i64> = tx
            .prepare_cached("SELECT id FROM dirs WHERE path=?1")
            .map_err(|e| e.to_string())?
            .query_row([parent_key], |row| row.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(id) = id {
            caches.dirs.insert(parent_key.to_string(), id);
        }
        id
    };
    let subtree_prefix = index_path_prefix(&path);
    let subtree_end = format!("{}0", subtree_prefix.trim_end_matches('/'));
    let is_dir = is_dir_hint || caches.dirs.contains_key(&path) || {
        tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM dirs WHERE path=?1)")
            .map_err(|e| e.to_string())?
            .query_row([&path], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
            > 0
    };
    if is_dir {
        tx.prepare_cached(
            "DELETE FROM entries WHERE dir_id IN
             (SELECT id FROM dirs WHERE path=?1 OR (path>=?2 AND path<?3))
             OR (dir_id=?4 AND name_id=?5)",
        )
        .map_err(|e| e.to_string())?
        .execute(params![
            path,
            subtree_prefix,
            subtree_end,
            parent_id,
            name_id
        ])
        .map_err(|e| e.to_string())?;
        tx.prepare_cached("DELETE FROM dirs WHERE path=?1 OR (path>=?2 AND path<?3)")
            .map_err(|e| e.to_string())?
            .execute(params![path, subtree_prefix, subtree_end])
            .map_err(|e| e.to_string())?;
        caches.invalidate_dirs_below(&path);
    } else if let (Some(parent_id), Some(name_id)) = (parent_id, name_id) {
        tx.prepare_cached("DELETE FROM entries WHERE dir_id=?1 AND name_id=?2")
            .map_err(|e| e.to_string())?
            .execute(params![parent_id, name_id])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn move_indexed_directory(
    tx: &Transaction<'_>,
    old_path: &Path,
    new_path: &Path,
    actor: &Actor,
    event_kind: i64,
    caches: &mut DbCaches,
) -> Result<bool, String> {
    let old = normalize_index_dir(old_path);
    let new = normalize_index_dir(new_path);
    if old == "/" || old == new {
        return Ok(false);
    }
    if !fs::symlink_metadata(new_path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
    {
        return Ok(false);
    }
    let indexed: bool = tx
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM dirs WHERE path=?1)")
        .map_err(|e| e.to_string())?
        .query_row([&old], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if !indexed {
        return Ok(false);
    }

    // A filesystem rename preserves every descendant. Rewrite the pooled
    // directory prefix instead of deleting and restating the whole subtree.
    remove_path(tx, new_path, true, caches)?;
    remove_exact_entry(tx, &old, caches)?;
    let old_prefix = index_path_prefix(&old);
    let old_end = format!("{}0", old_prefix.trim_end_matches('/'));
    tx.prepare_cached(
        "UPDATE dirs
         SET path=CASE WHEN path=?1 THEN ?2
                       ELSE ?2 || substr(path, length(?1) + 1) END
         WHERE path=?1 OR (path>=?3 AND path<?4)",
    )
    .map_err(|e| e.to_string())?
    .execute(params![old, new, old_prefix, old_end])
    .map_err(|e| e.to_string())?;
    caches.invalidate_dirs_below(&old);
    caches.invalidate_dirs_below(&new);
    Ok(upsert_path(tx, new_path, true, actor, event_kind, caches)? == IndexedPathState::Directory)
}

fn remove_exact_entry(
    tx: &Transaction<'_>,
    path: &str,
    caches: &mut DbCaches,
) -> Result<(), String> {
    let path = Path::new(path);
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(name) = path.file_name() else {
        return Ok(());
    };
    let parent = normalize_index_dir(parent);
    let name = name.to_string_lossy();
    tx.prepare_cached(
        "DELETE FROM entries
         WHERE dir_id=(SELECT id FROM dirs WHERE path=?1)
           AND name_id=(SELECT id FROM strings WHERE value=?2)",
    )
    .map_err(|e| e.to_string())?
    .execute(params![parent, name.as_ref()])
    .map_err(|e| e.to_string())?;
    caches.invalidate_dirs_below(path.to_string_lossy().as_ref());
    Ok(())
}

fn reconcile_subtree(
    conn: &mut Connection,
    path: &Path,
    opts: &Options,
    caches: &mut DbCaches,
) -> Result<(), String> {
    let path = path.to_path_buf();
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    if !metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_dir())
    {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        if metadata.is_some() {
            let _ = upsert_path(
                &tx,
                &path,
                false,
                &Actor::reconcile(),
                EVENT_RECONCILE,
                caches,
            )?;
        } else {
            remove_path(&tx, &path, true, caches)?;
        }
        return tx.commit().map_err(|e| e.to_string());
    }
    let root_key = normalize_index_dir(&path);
    let threads = if root_prefers_single_thread(&path) {
        1
    } else {
        opts.threads_override.max(1)
    };
    let entries = scan_index_root_cancellable(&path, &root_key, threads, Some(&WATCH_STOP))?;
    reconcile_subtree_from_entries(conn, &path, entries, caches)
}

fn reconcile_subtree_from_entries(
    conn: &mut Connection,
    path: &Path,
    entries: Vec<ScannedIndexEntry>,
    caches: &mut DbCaches,
) -> Result<(), String> {
    let root_key = normalize_index_dir(path);
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    tx.prepare_cached(
        "DELETE FROM entries WHERE dir_id IN
         (SELECT id FROM dirs WHERE path=?1 OR path LIKE ?2 ESCAPE '\\')",
    )
    .map_err(|e| e.to_string())?
    .execute(params![
        root_key,
        format!("{}/%", sql_like_escape(&root_key))
    ])
    .map_err(|e| e.to_string())?;
    tx.prepare_cached("DELETE FROM dirs WHERE path LIKE ?1 ESCAPE '\\'")
        .map_err(|e| e.to_string())?
        .execute([format!("{}/%", sql_like_escape(&root_key))])
        .map_err(|e| e.to_string())?;
    caches.invalidate_dirs_below(&root_key);
    ensure_dir_cached(&tx, &root_key, caches)?;
    let actor = Actor::reconcile();
    for entry in entries {
        let path = fsx::decode_lossless_path(&entry.path);
        let metadata = fs::symlink_metadata(&path).ok();
        if metadata.is_some() {
            let _ = upsert_path(&tx, &path, entry.kind == 1, &actor, EVENT_RECONCILE, caches)?;
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

fn start_backend(roots: &[RootState], opts: &Options) -> Result<StartedBackend, String> {
    match start_fanotify(roots) {
        Ok((rx, name)) => Ok((rx, name, None, HashMap::new())),
        Err(fanotify_error) => {
            let (rx, scans) = start_inotify(roots, opts)?;
            Ok((rx, "inotify".to_string(), Some(fanotify_error), scans))
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
        scanned_entries: None,
        snapshot_lease: None,
    }
}

fn classify_actor(pid: i32) -> Actor {
    if pid <= 0 {
        return Actor::unknown();
    }
    static ACTOR_CACHE: OnceLock<Mutex<HashMap<(i32, i64), Actor>>> = OnceLock::new();
    let starttime = process_starttime(i64::from(pid)).unwrap_or_default();
    if starttime != 0 {
        if let Ok(cache) = ACTOR_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        {
            if let Some(actor) = cache.get(&(pid, starttime)) {
                return actor.clone();
            }
        }
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
    let actor = Actor {
        executable,
        classification: classification.to_string(),
        uid,
        pid: Some(i64::from(pid)),
    };
    if starttime != 0 {
        if let Ok(mut cache) = ACTOR_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        {
            cache.insert((pid, starttime), actor.clone());
            if cache.len() > 4096 {
                cache.retain(|(cached_pid, cached_start), _| {
                    process_starttime(i64::from(*cached_pid)) == Some(*cached_start)
                });
            }
        }
    }
    actor
}

#[cfg(target_os = "linux")]
fn start_inotify(
    roots: &[RootState],
    opts: &Options,
) -> Result<(EventReceiver, InitialScans), String> {
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
        pending_self_moves: HashMap::new(),
        recently_rebased: HashMap::new(),
        mounted: roots
            .iter()
            .flat_map(|root| mount_candidates(&root.path))
            .collect(),
        last_mount_check: std::time::Instant::now(),
    };
    let mut scans = HashMap::new();
    for root in roots {
        if WATCH_STOP.load(Ordering::Relaxed) {
            return Err("watcher interrupted during initial watch scan".to_string());
        }
        match watcher.add_recursive_collect(&root.path) {
            Ok(entries) => {
                scans.insert(root.key.clone(), entries);
            }
            Err(error) => {
                return Err(error);
            }
        }
    }
    for root in roots {
        if let Some(entries) = scans.get_mut(&root.key) {
            let threads = if root_prefers_single_thread(&root.path) {
                1
            } else {
                opts.threads_override.max(1)
            };
            populate_scanned_index_metadata_cancellable(entries, threads, Some(&WATCH_STOP))?;
        }
    }
    if WATCH_STOP.load(Ordering::Relaxed) {
        return Err("watcher interrupted during initial metadata scan".to_string());
    }
    let (tx, rx) = event_queue(WATCH_CHANNEL_CAPACITY);
    thread::Builder::new()
        .name("unearth-inotify".to_string())
        .spawn(move || {
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| watcher.run(tx)))
            {
                eprintln!(
                    "unearth: inotify backend panicked: {}",
                    panic_text(&payload)
                );
            }
        })
        .map_err(|e| e.to_string())?;
    Ok((rx, scans))
}

#[cfg(not(target_os = "linux"))]
fn start_inotify(
    _roots: &[RootState],
    _opts: &Options,
) -> Result<(EventReceiver, InitialScans), String> {
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
    paths: HashMap<i32, Arc<Path>>,
    path_to_wd: BTreeMap<Arc<Path>, i32>,
    pending_moves: HashMap<u32, PendingMove>,
    pending_self_moves: HashMap<i32, (PathBuf, std::time::Instant)>,
    recently_rebased: HashMap<i32, std::time::Instant>,
    mounted: HashSet<PathBuf>,
    last_mount_check: std::time::Instant,
}

#[cfg(target_os = "linux")]
fn wait_for_backend_event(fd: RawFd) -> Result<(), std::io::Error> {
    let timeout_ms = WATCH_BACKEND_WAIT.as_millis().min(i32::MAX as u128) as i32;
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if result >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(target_os = "linux")]
impl InotifyWatcher {
    fn expire_pending_moves(&mut self, tx: &EventSender) {
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
        let expired_self_moves: Vec<i32> = self
            .pending_self_moves
            .iter()
            .filter_map(|(wd, (_, seen_at))| {
                (now.duration_since(*seen_at) >= PENDING_MOVE_TIMEOUT).then_some(*wd)
            })
            .collect();
        for wd in expired_self_moves {
            if let Some((path, _)) = self.pending_self_moves.remove(&wd) {
                let _ = tx.send(event(
                    Action::Overflow,
                    path,
                    true,
                    "inotify",
                    Actor::unknown(),
                ));
            }
        }
        self.recently_rebased
            .retain(|_, seen_at| now.duration_since(*seen_at) < PENDING_MOVE_TIMEOUT);
    }

    fn add_watch(&mut self, path: &Path) -> Result<(), String> {
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("cannot watch path containing NUL: {}", path.display()))?;
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_MODIFY
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
        let shared_path = Arc::<Path>::from(path);
        if let Some(previous_path) = self.paths.insert(wd, Arc::clone(&shared_path)) {
            if previous_path.as_ref() != path {
                self.path_to_wd.remove(&previous_path);
            }
        }
        if let Some(previous_wd) = self.path_to_wd.insert(shared_path, wd) {
            if previous_wd != wd {
                self.paths.remove(&previous_wd);
            }
        }
        Ok(())
    }

    fn add_recursive(&mut self, root: &Path) -> Result<(), String> {
        self.walk_recursive(root, false, None, None).map(|_| ())
    }

    fn add_recursive_collect(&mut self, root: &Path) -> Result<Vec<ScannedIndexEntry>, String> {
        self.walk_recursive(root, true, Some(&WATCH_STOP), None)
            .map(|entries| entries.expect("unbounded initial scan is retained"))
    }

    fn add_recursive_snapshot(
        &mut self,
        root: &Path,
    ) -> Result<Option<Vec<ScannedIndexEntry>>, String> {
        self.walk_recursive(root, true, Some(&WATCH_STOP), Some(WATCH_SNAPSHOT_BYTES))
    }

    fn walk_recursive(
        &mut self,
        root: &Path,
        collect_entries: bool,
        cancel: Option<&AtomicBool>,
        limit: Option<usize>,
    ) -> Result<Option<Vec<ScannedIndexEntry>>, String> {
        let root_key = normalize_index_dir(root);
        let scan_root = root.to_path_buf();
        let mut scanned = collect_entries.then(Vec::new);
        let mut path_bytes = 0usize;
        let mut pending = vec![scan_root.clone()];
        while let Some(directory) = pending.pop() {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return Err("watcher interrupted during initial watch scan".to_string());
            }
            let directory_key = normalize_index_dir(&directory);
            let Some(excluded) = matching_root(&self.roots, &directory)
                .map(|matched| is_root_index_excluded_path(&matched.key, &directory_key))
            else {
                continue;
            };
            if excluded {
                continue;
            }

            // Install coverage before enumerating children. Anything created
            // after read_dir begins is then either scanned or queued by inotify.
            self.add_watch(&directory)?;
            let children = match fs::read_dir(&directory) {
                Ok(children) => children,
                Err(error) if transient_watch_path_error(&error) => continue,
                Err(error) => return Err(format!("cannot read {}: {error}", directory.display())),
            };
            for child in children {
                if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    return Err("watcher interrupted during initial watch scan".to_string());
                }
                let child = match child {
                    Ok(child) => child,
                    Err(error) if transient_watch_path_error(&error) => continue,
                    Err(error) => return Err(error.to_string()),
                };
                let path = child.path();
                if is_root_index_prune_child(&root_key, &path) {
                    continue;
                }
                let path_key = normalize_index_dir(&path);
                let Some(excluded) = matching_root(&self.roots, &path)
                    .map(|matched| is_root_index_excluded_path(&matched.key, &path_key))
                else {
                    continue;
                };
                if excluded {
                    continue;
                }
                let file_type = match child.file_type() {
                    Ok(file_type) => file_type,
                    Err(error) if transient_watch_path_error(&error) => continue,
                    Err(error) => return Err(error.to_string()),
                };
                if file_type.is_dir() {
                    pending.push(path.clone());
                }
                if let Some(entries) = scanned
                    .as_mut()
                    .filter(|_| path != scan_root && path.file_name().is_some())
                {
                    path_bytes = path_bytes.saturating_add(path_key.capacity());
                    entries.push(ScannedIndexEntry {
                        path: path_key,
                        kind: if file_type.is_dir() {
                            1
                        } else if file_type.is_symlink() {
                            2
                        } else {
                            0
                        },
                        mtime: None,
                        size: None,
                        allocated_size: None,
                        activity: None,
                        device: None,
                        inode: None,
                        link_count: None,
                    });
                    if limit.is_some_and(|limit| {
                        path_bytes.saturating_add(
                            entries
                                .capacity()
                                .saturating_mul(std::mem::size_of::<ScannedIndexEntry>()),
                        ) > limit
                    }) {
                        // Continue installing all watches, but do not publish a
                        // truncated scan as if it described the whole subtree.
                        scanned = None;
                    }
                }
            }
        }
        Ok(scanned)
    }

    fn remove_watches_below(&mut self, path: &Path) {
        let doomed: Vec<i32> = self
            .paths
            .iter()
            .filter_map(|(wd, watched)| watched.starts_with(path).then_some(*wd))
            .collect();
        for wd in doomed {
            self.pending_self_moves.remove(&wd);
            self.recently_rebased.remove(&wd);
            if let Some(watched) = self.paths.remove(&wd) {
                self.path_to_wd.remove(&watched);
            }
            unsafe {
                libc::inotify_rm_watch(self.fd, wd);
            }
        }
    }

    fn update_watches_after_move(&mut self, old: &Path, new: &Path) -> bool {
        let upper = old.join("\u{10ffff}");
        let moved: Vec<(i32, Arc<Path>, Arc<Path>)> = self
            .path_to_wd
            .range(Arc::<Path>::from(old)..Arc::<Path>::from(upper))
            .filter_map(|(path, wd)| {
                path.strip_prefix(old)
                    .ok()
                    .map(|suffix| (*wd, Arc::clone(path), Arc::<Path>::from(new.join(suffix))))
            })
            .collect();
        let had_watch_coverage = !moved.is_empty();
        let now = std::time::Instant::now();
        for (wd, old_path, new_path) in moved {
            self.pending_self_moves.remove(&wd);
            self.recently_rebased.insert(wd, now);
            self.path_to_wd.remove(&old_path);
            self.path_to_wd.insert(Arc::clone(&new_path), wd);
            self.paths.insert(wd, new_path);
        }
        had_watch_coverage
    }

    fn poll_mounts(&mut self, tx: &EventSender) -> Result<(), String> {
        if self.last_mount_check.elapsed() < WATCH_MOUNT_CHECK_INTERVAL {
            return Ok(());
        }
        let current: HashSet<PathBuf> = self
            .roots
            .iter()
            .flat_map(|root| mount_candidates(&root.path))
            .collect();
        let added: Vec<PathBuf> = current.difference(&self.mounted).cloned().collect();
        let removed: Vec<PathBuf> = self.mounted.difference(&current).cloned().collect();
        let mut failed_adds = HashSet::new();
        for mount in added {
            match self.add_recursive_snapshot(&mount) {
                Ok(entries) => {
                    let mut reconcile = event(
                        Action::Reconcile,
                        mount.clone(),
                        true,
                        "inotify",
                        Actor::unknown(),
                    );
                    reconcile.scanned_entries = entries.map(Arc::new);
                    let _ = tx.send(reconcile);
                }
                Err(error) => {
                    self.remove_watches_below(&mount);
                    failed_adds.insert(mount.clone());
                    let _ = tx.send(event(
                        Action::Overflow,
                        mount.clone(),
                        true,
                        "inotify",
                        Actor::unknown(),
                    ));
                    eprintln!(
                        "unearth: cannot watch newly mounted {}; will retry: {error}",
                        mount.display()
                    );
                }
            }
        }
        for mount in removed {
            self.remove_watches_below(&mount);
            let _ = tx.send(event(
                Action::Overflow,
                mount,
                true,
                "inotify",
                Actor::unknown(),
            ));
        }
        self.mounted = current.difference(&failed_adds).cloned().collect();
        self.last_mount_check = std::time::Instant::now();
        Ok(())
    }

    fn run(mut self, tx: EventSender) {
        let mut buffer = vec![0u8; 1024 * 1024];
        'watch: loop {
            self.expire_pending_moves(&tx);
            if let Err(error) = self.poll_mounts(&tx) {
                let _ = tx.send(event(
                    Action::Overflow,
                    PathBuf::from("/"),
                    true,
                    "inotify",
                    Actor::unknown(),
                ));
                eprintln!("unearth: inotify mount coverage failed: {error}");
                break;
            }
            let read = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    if let Err(error) = wait_for_backend_event(self.fd) {
                        let _ = tx.send(event(
                            Action::Overflow,
                            PathBuf::from("/"),
                            true,
                            "inotify",
                            Actor::unknown(),
                        ));
                        eprintln!("unearth: inotify wait failed: {error}");
                        break;
                    }
                    continue;
                }
                if error.kind() == std::io::ErrorKind::Interrupted {
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
                    self.pending_self_moves.remove(&wd);
                    self.recently_rebased.remove(&wd);
                    if let Some(ignored) = self.paths.remove(&wd) {
                        self.path_to_wd.remove(&ignored);
                    }
                }
                let name = buffer[offset + 16..offset + record_len]
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap_or_default();
                let path = base.map(|base| {
                    if name.is_empty() {
                        base.to_path_buf()
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
                    let roots = self.roots.clone();
                    for root in roots {
                        if let Err(error) = self.add_recursive(&root.path) {
                            eprintln!(
                                "unearth: cannot restore watch coverage after queue overflow: {error}"
                            );
                            break 'watch;
                        }
                    }
                    // The first recovery marks the index dirty while watches are
                    // rebuilt; this second one closes the reconstruction window.
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
                let actionable_mask = libc::IN_CREATE
                    | libc::IN_DELETE
                    | libc::IN_MOVED_FROM
                    | libc::IN_MOVED_TO
                    | libc::IN_MODIFY
                    | libc::IN_CLOSE_WRITE
                    | libc::IN_ATTRIB
                    | libc::IN_DELETE_SELF
                    | libc::IN_MOVE_SELF
                    | libc::IN_UNMOUNT;
                if mask & libc::IN_IGNORED != 0 && mask & actionable_mask == 0 {
                    if let Some(path) = path.as_deref() {
                        if path.is_dir() {
                            if let Err(error) = self.add_recursive(path) {
                                let _ = tx.send(event(
                                    Action::Overflow,
                                    path.to_path_buf(),
                                    true,
                                    "inotify",
                                    Actor::unknown(),
                                ));
                                eprintln!(
                                    "unearth: cannot restore unexpectedly removed watch for {}: {error}",
                                    path.display()
                                );
                                break 'watch;
                            }
                        }
                        let _ = tx.send(event(
                            Action::Reconcile,
                            path.to_path_buf(),
                            true,
                            "inotify",
                            Actor::unknown(),
                        ));
                    }
                    offset += record_len;
                    continue;
                }
                let Some(path) = path else {
                    if mask & actionable_mask != 0 {
                        let _ = tx.send(event(
                            Action::Overflow,
                            PathBuf::from("/"),
                            true,
                            "inotify",
                            Actor::unknown(),
                        ));
                    }
                    offset += record_len;
                    continue;
                };
                let is_dir = mask & libc::IN_ISDIR != 0;
                if mask & (libc::IN_UNMOUNT | libc::IN_DELETE_SELF) != 0 {
                    let _ = tx.send(event(
                        Action::Overflow,
                        path,
                        true,
                        "inotify",
                        Actor::unknown(),
                    ));
                } else if mask & libc::IN_MOVE_SELF != 0 {
                    if self.recently_rebased.remove(&wd).is_none() {
                        self.pending_self_moves
                            .insert(wd, (path, std::time::Instant::now()));
                    }
                } else if mask & libc::IN_MOVED_FROM != 0 {
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
                        let destination_is_watched =
                            matching_root(&self.roots, &path).is_some_and(|root| {
                                !is_root_index_excluded_path(&root.key, &normalize_index_dir(&path))
                            });
                        if destination_is_watched {
                            let watch_coverage_rebased =
                                self.update_watches_after_move(&old.path, &path);
                            if (is_dir || old.is_dir) && !watch_coverage_rebased {
                                if let Err(error) = self.add_recursive(&path) {
                                    let _ = tx.send(event(
                                        Action::Overflow,
                                        path.clone(),
                                        true,
                                        "inotify",
                                        Actor::unknown(),
                                    ));
                                    eprintln!(
                                        "unearth: cannot cover moved directory {}: {error}",
                                        path.display()
                                    );
                                    break 'watch;
                                }
                            }
                        } else if old.is_dir {
                            self.remove_watches_below(&old.path);
                        }
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
                        if is_dir {
                            let entries = match self.add_recursive_snapshot(&path) {
                                Ok(entries) => entries,
                                Err(error) => {
                                    let _ = tx.send(event(
                                        Action::Overflow,
                                        path.clone(),
                                        true,
                                        "inotify",
                                        Actor::unknown(),
                                    ));
                                    eprintln!(
                                        "unearth: cannot watch moved-in {}: {error}",
                                        path.display()
                                    );
                                    break 'watch;
                                }
                            };
                            let mut reconcile = event(
                                Action::Reconcile,
                                path.clone(),
                                true,
                                "inotify",
                                Actor::unknown(),
                            );
                            reconcile.scanned_entries = entries.map(Arc::new);
                            let _ = tx.send(reconcile);
                        } else {
                            let _ = tx.send(event(
                                Action::Reconcile,
                                path.clone(),
                                is_dir,
                                "inotify",
                                Actor::unknown(),
                            ));
                        }
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
                        let entries = match self.add_recursive_snapshot(&path) {
                            Ok(entries) => entries,
                            Err(error) => {
                                let _ = tx.send(event(
                                    Action::Overflow,
                                    path.clone(),
                                    true,
                                    "inotify",
                                    Actor::unknown(),
                                ));
                                eprintln!(
                                    "unearth: cannot watch new directory {}: {error}",
                                    path.display()
                                );
                                break 'watch;
                            }
                        };
                        // The watch walk also discovers the initial subtree. Reuse
                        // those entries during reconciliation instead of walking it
                        // a second time; later child events invalidate this snapshot.
                        let mut reconcile = event(
                            Action::Reconcile,
                            path.clone(),
                            true,
                            "inotify",
                            Actor::unknown(),
                        );
                        reconcile.scanned_entries = entries.map(Arc::new);
                        let _ = tx.send(reconcile);
                    }
                    if !is_dir {
                        let _ = tx.send(event(
                            Action::Upsert,
                            path,
                            is_dir,
                            "inotify",
                            Actor::unknown(),
                        ));
                    }
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
                } else if mask & (libc::IN_MODIFY | libc::IN_CLOSE_WRITE) != 0 {
                    let mut changed =
                        event(Action::Upsert, path, is_dir, "inotify", Actor::unknown());
                    changed.event_kind = EVENT_MODIFY;
                    let _ = tx.send(changed);
                } else if mask & libc::IN_ATTRIB != 0 {
                    if is_dir {
                        if let Err(error) = self.add_recursive(&path) {
                            let _ = tx.send(event(
                                Action::Overflow,
                                path.clone(),
                                true,
                                "inotify",
                                Actor::unknown(),
                            ));
                            eprintln!(
                                "unearth: cannot restore directory coverage for {}: {error}",
                                path.display()
                            );
                            break 'watch;
                        }
                    }
                    let mut changed =
                        event(Action::Upsert, path, is_dir, "inotify", Actor::unknown());
                    changed.event_kind = EVENT_ATTRIB;
                    let _ = tx.send(changed);
                }
                offset += record_len;
            }
        }
        let fd = self.fd;
        self.fd = -1;
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for InotifyWatcher {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn start_fanotify(_roots: &[RootState]) -> Result<(EventReceiver, String), String> {
    Err("fanotify is only available on Linux".to_string())
}

#[cfg(target_os = "linux")]
fn start_fanotify(roots: &[RootState]) -> Result<(EventReceiver, String), String> {
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
    let mut mounts: Vec<MountHandle> = Vec::new();
    let mut marked = HashSet::new();
    let mut mount_ids = HashMap::new();
    for root in roots {
        let candidates = mount_candidates(&root.path);
        for mount in candidates {
            if !marked.insert(mount.clone()) {
                continue;
            }
            let c_path = match CString::new(mount.as_os_str().as_bytes()) {
                Ok(path) => path,
                Err(_) => {
                    for handle in mounts {
                        unsafe {
                            libc::close(handle.fd);
                        }
                    }
                    unsafe {
                        libc::close(fd);
                    }
                    return Err(format!(
                        "cannot watch mount path containing NUL: {}",
                        mount.display()
                    ));
                }
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
                for handle in mounts {
                    unsafe {
                        libc::close(handle.fd);
                    }
                }
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
            } else {
                let error = std::io::Error::last_os_error();
                for handle in mounts {
                    unsafe {
                        libc::close(handle.fd);
                    }
                }
                unsafe {
                    libc::close(fd);
                }
                return Err(format!(
                    "cannot open marked mount {}: {error}",
                    mount.display()
                ));
            }
        }
    }
    if mounts.is_empty() {
        unsafe {
            libc::close(fd);
        }
        return Err("no fanotify mount could be opened".to_string());
    }
    let (tx, rx) = event_queue(WATCH_CHANNEL_CAPACITY);
    let roots = roots.to_vec();
    let watcher = FanotifyWatcher {
        fd,
        roots,
        mounts,
        marked,
        fsid_mounts: HashMap::new(),
        mount_ids,
    };
    thread::Builder::new()
        .name("unearth-fanotify".to_string())
        .spawn(move || {
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| watcher.run(tx)))
            {
                eprintln!(
                    "unearth: fanotify backend panicked: {}",
                    panic_text(&payload)
                );
            }
        })
        .map_err(|e| e.to_string())?;
    Ok((rx, "fanotify".to_string()))
}

fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
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
        | libc::FAN_FS_ERROR
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
    fn add_new_mounts(&mut self, tx: &EventSender) -> Result<(), String> {
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
            let _ = tx.send(event(
                Action::Overflow,
                mount,
                true,
                "fanotify",
                Actor::unknown(),
            ));
        }
        for root in &self.roots {
            for mount in mount_candidates(&root.path) {
                if self.marked.contains(&mount)
                    && self.mount_ids.get(&mount) == current_ids.get(&mount)
                {
                    continue;
                }
                let c_path = match CString::new(mount.as_os_str().as_bytes()) {
                    Ok(path) => path,
                    Err(_) => {
                        let _ = tx.send(event(
                            Action::Overflow,
                            mount.clone(),
                            true,
                            "fanotify",
                            Actor::unknown(),
                        ));
                        eprintln!(
                            "unearth: cannot watch mount path containing NUL {}; will retry",
                            mount.display()
                        );
                        continue;
                    }
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
                    let error = std::io::Error::last_os_error();
                    let _ = tx.send(event(
                        Action::Overflow,
                        mount.clone(),
                        true,
                        "fanotify",
                        Actor::unknown(),
                    ));
                    eprintln!(
                        "unearth: cannot mark newly mounted {}; will retry: {error}",
                        mount.display()
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
                    let error = std::io::Error::last_os_error();
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
                    let _ = tx.send(event(
                        Action::Overflow,
                        mount.clone(),
                        true,
                        "fanotify",
                        Actor::unknown(),
                    ));
                    eprintln!(
                        "unearth: cannot open newly mounted {}; will retry: {error}",
                        mount.display()
                    );
                }
            }
        }
        Ok(())
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

    fn run(mut self, tx: EventSender) {
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut last_mount_check = std::time::Instant::now();
        'watch: loop {
            if last_mount_check.elapsed() >= WATCH_MOUNT_CHECK_INTERVAL {
                if let Err(error) = self.add_new_mounts(&tx) {
                    let _ = tx.send(event(
                        Action::Overflow,
                        PathBuf::from("/"),
                        true,
                        "fanotify",
                        Actor::unknown(),
                    ));
                    eprintln!("unearth: fanotify mount coverage failed: {error}");
                    break;
                }
                last_mount_check = std::time::Instant::now();
            }
            let read = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    if let Err(error) = wait_for_backend_event(self.fd) {
                        let _ = tx.send(event(
                            Action::Overflow,
                            PathBuf::from("/"),
                            true,
                            "fanotify",
                            Actor::unknown(),
                        ));
                        eprintln!("unearth: fanotify wait failed: {error}");
                        break 'watch;
                    }
                    continue;
                }
                if error.kind() == std::io::ErrorKind::Interrupted {
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
                if mask & (libc::FAN_Q_OVERFLOW | libc::FAN_FS_ERROR) != 0 {
                    let _ = tx.send(event(
                        Action::Overflow,
                        PathBuf::from("/"),
                        true,
                        "fanotify",
                        classify_actor(pid),
                    ));
                    if let Err(error) = self.add_new_mounts(&tx) {
                        eprintln!(
                            "unearth: cannot restore fanotify coverage after overflow: {error}"
                        );
                        break 'watch;
                    }
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
        for mount in self.mounts.drain(..) {
            unsafe {
                libc::close(mount.fd);
            }
        }
        let fd = self.fd;
        self.fd = -1;
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for FanotifyWatcher {
    fn drop(&mut self) {
        for mount in self.mounts.drain(..) {
            unsafe {
                libc::close(mount.fd);
            }
        }
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
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
    let target = normalize_index_dir(path);
    if let Some(id) = cached_mount_identities().get(&target) {
        return Some(*id);
    }
    fs::metadata(path).ok().map(|metadata| metadata.dev())
}

#[cfg(target_os = "linux")]
fn cached_mount_identities() -> HashMap<String, u64> {
    type MountIdentityCache = Option<(Instant, HashMap<String, u64>)>;
    static CACHE: OnceLock<Mutex<MountIdentityCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let Ok(mut guard) = cache.lock() else {
        return HashMap::new();
    };
    if let Some((created, identities)) = guard.as_ref() {
        if created.elapsed() < Duration::from_secs(5) {
            return identities.clone();
        }
    }
    let mut identities = HashMap::new();
    if let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") {
        for line in mountinfo.lines() {
            let mut fields = line.split_whitespace();
            let Some(raw_id) = fields.next() else {
                continue;
            };
            let _ = fields.next();
            let _ = fields.next();
            let _ = fields.next();
            let Some(raw_mount) = fields.next() else {
                continue;
            };
            if let Ok(id) = raw_id.parse() {
                identities.insert(
                    normalize_index_dir(Path::new(&fsx::mount::unescape_mount_field(raw_mount))),
                    id,
                );
            }
        }
    }
    *guard = Some((Instant::now(), identities.clone()));
    identities
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
        let mount = PathBuf::from(fsx::mount::unescape_mount_field(raw_mount));
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

    #[cfg(target_os = "linux")]
    #[test]
    fn capped_snapshot_keeps_watch_coverage_and_can_rescan_every_entry() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("target/nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("file"), b"fresh").unwrap();
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let mut watcher = InotifyWatcher {
            fd,
            roots: vec![RootState {
                key: normalize_index_dir(root.path()),
                path: root.path().into(),
            }],
            paths: HashMap::new(),
            path_to_wd: BTreeMap::new(),
            pending_moves: HashMap::new(),
            pending_self_moves: HashMap::new(),
            recently_rebased: HashMap::new(),
            mounted: HashSet::new(),
            last_mount_check: std::time::Instant::now(),
        };
        assert!(watcher
            .walk_recursive(root.path(), true, None, Some(1))
            .unwrap()
            .is_none());
        assert_eq!(watcher.paths.len(), 3);
        assert!(watcher.path_to_wd.contains_key(nested.as_path()));
        let entries = watcher
            .walk_recursive(root.path(), true, None, None)
            .unwrap()
            .unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.path == normalize_index_dir(&nested.join("file"))));
    }

    #[test]
    fn snapshot_budget_exhaustion_retains_full_reconciliation_event() {
        let (mut tx, rx) = event_queue(4);
        tx.snapshot_budget = SnapshotBudget::new(0);
        let mut reconcile = event(
            Action::Reconcile,
            PathBuf::from("/tmp/snapshot-budget"),
            true,
            "test",
            Actor::unknown(),
        );
        reconcile.scanned_entries = Some(Arc::new(Vec::with_capacity(1)));
        tx.send(reconcile).unwrap();
        let retained = rx.inner.recv().unwrap();
        assert_eq!(retained.action, Action::Reconcile);
        assert!(retained.scanned_entries.is_none());
        assert!(retained.snapshot_lease.is_none());
    }

    #[test]
    fn reconciliation_ancestry_handles_many_disjoint_paths_and_both_move_sides() {
        let paths: HashSet<_> = (0..4096)
            .flat_map(|i| {
                [
                    PathBuf::from(format!("/r/d{i}")),
                    PathBuf::from(format!("/r/d{i}/target/nested")),
                ]
            })
            .collect();
        assert_eq!(collapse_reconcile_paths(paths).len(), 4096);
        let mut scans = HashMap::from_iter(["/r/a", "/r/b", "/r/ab"].map(|p| {
            (
                PathBuf::from(p),
                ReconcileScan {
                    entries: Arc::new(Vec::new()),
                    _lease: None,
                },
            )
        }));
        let mut moved = event(
            Action::Move,
            PathBuf::from("/r/b/new"),
            false,
            "test",
            Actor::unknown(),
        );
        moved.old_path = Some(PathBuf::from("/r/a/old"));
        invalidate_stale_reconcile_scans(&[moved], &mut scans);
        assert_eq!(scans.len(), 1);
        assert!(scans.contains_key(Path::new("/r/ab")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inotify_directory_move_reuses_pooled_watch_paths() {
        let old = PathBuf::from("/tmp/unearth-watch-old");
        let child = old.join("child");
        let new = PathBuf::from("/tmp/unearth-watch-new");
        let old_shared = Arc::<Path>::from(old.as_path());
        let child_shared = Arc::<Path>::from(child.as_path());
        let mut watcher = InotifyWatcher {
            fd: -1,
            roots: Vec::new(),
            paths: HashMap::from([(1, Arc::clone(&old_shared)), (2, Arc::clone(&child_shared))]),
            path_to_wd: BTreeMap::from([(old_shared, 1), (child_shared, 2)]),
            pending_moves: HashMap::new(),
            pending_self_moves: HashMap::from([
                (1, (old.clone(), std::time::Instant::now())),
                (2, (child, std::time::Instant::now())),
            ]),
            recently_rebased: HashMap::new(),
            mounted: HashSet::new(),
            last_mount_check: std::time::Instant::now(),
        };

        assert!(watcher.update_watches_after_move(&old, &new));
        for (wd, expected) in [(1, new.clone()), (2, new.join("child"))] {
            let shared = watcher.paths.get(&wd).unwrap();
            assert_eq!(shared.as_ref(), expected);
            assert_eq!(watcher.path_to_wd.get(shared), Some(&wd));
            assert_eq!(Arc::strong_count(shared), 2);
            assert!(!watcher.pending_self_moves.contains_key(&wd));
            assert!(watcher.recently_rebased.contains_key(&wd));
        }
    }

    fn live_entry_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE strings (
                 id INTEGER PRIMARY KEY,
                 value TEXT NOT NULL UNIQUE
             );
             CREATE TABLE dirs (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE
             );
             CREATE TABLE actors (
                 id INTEGER PRIMARY KEY,
                 executable TEXT NOT NULL UNIQUE,
                 classification TEXT NOT NULL,
                 first_seen INTEGER NOT NULL,
                 last_seen INTEGER NOT NULL
             );
             CREATE TABLE entries (
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
                 actor_uid INTEGER,
                 actor_pid INTEGER,
                 event_at INTEGER,
                 UNIQUE(dir_id, name_id, kind)
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn event_queue_coalesces_only_pending_modify_events() {
        let path = PathBuf::from("/tmp/unearth-queue-test");
        let (tx, rx) = event_queue(4);
        let mut first = event(
            Action::Upsert,
            path.clone(),
            false,
            "test",
            Actor::unknown(),
        );
        first.event_kind = EVENT_MODIFY;
        tx.send(first.clone()).unwrap();
        tx.send(first.clone()).unwrap();
        assert_eq!(rx.inner.len(), 1);

        let dequeued = rx.try_recv().unwrap();
        assert_eq!(dequeued.path, path);
        tx.send(first).unwrap();
        assert_eq!(rx.inner.len(), 1);
    }

    #[test]
    fn clock_cache_remains_bounded_and_keeps_new_entries() {
        let mut cache = ClockCache::new(2);
        cache.insert("old".to_string(), 1);
        cache.insert("hot".to_string(), 2);
        cache.insert("new".to_string(), 3);
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.get("new"), Some(&3));
        assert!(!cache.contains_key("old"));
    }

    #[test]
    fn batch_state_touch_uses_latest_time_and_one_generation() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE watch_state (
                 root TEXT PRIMARY KEY,
                 generation INTEGER NOT NULL,
                 last_event INTEGER,
                 online INTEGER NOT NULL
             );
             INSERT INTO watch_state VALUES ('/tmp/root', 7, NULL, 0);",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        let mut touched = HashMap::new();
        record_state_touch(&mut touched, "/tmp/root", 2_000_000_000);
        record_state_touch(&mut touched, "/tmp/root", 1_000_000_000);
        touch_states(&tx, touched).unwrap();
        tx.commit().unwrap();
        let state: (i64, i64, i64) = conn
            .query_row(
                "SELECT generation, last_event, online FROM watch_state WHERE root='/tmp/root'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (8, 2, 1));
    }

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
    fn pre_scanned_reconcile_is_invalidated_by_child_event() {
        let root = PathBuf::from("/tmp/unearth-watch-test");
        let child = root.join("child");
        let mut reconcile = event(
            Action::Reconcile,
            root.clone(),
            true,
            "test",
            Actor::unknown(),
        );
        reconcile.scanned_entries = Some(Arc::new(Vec::new()));
        let batch = vec![
            reconcile,
            event(Action::Upsert, child, false, "test", Actor::unknown()),
        ];
        let mut scans = HashMap::from([(
            root,
            ReconcileScan {
                entries: Arc::new(Vec::new()),
                _lease: None,
            },
        )]);

        invalidate_stale_reconcile_scans(&batch, &mut scans);

        assert!(scans.is_empty());
    }

    #[test]
    fn coalescing_never_discards_overflow_recovery() {
        let path = PathBuf::from("/tmp/unearth-watch-test");
        let batch = vec![
            event(
                Action::Overflow,
                path.clone(),
                true,
                "test",
                Actor::unknown(),
            ),
            event(Action::Upsert, path, true, "test", Actor::unknown()),
        ];
        let result = coalesce_batch(batch);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].action, Action::Overflow);
    }

    #[test]
    fn coalescing_does_not_reorder_events_across_moves() {
        let old = PathBuf::from("/tmp/unearth-watch-old");
        let new = PathBuf::from("/tmp/unearth-watch-new");
        let mut moved = event(Action::Move, new.clone(), false, "test", Actor::unknown());
        moved.old_path = Some(old.clone());
        let batch = vec![
            event(Action::Upsert, old, false, "test", Actor::unknown()),
            moved,
            event(Action::Upsert, new, false, "test", Actor::unknown()),
        ];
        let result = coalesce_batch(batch);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].action, Action::Upsert);
        assert_eq!(result[1].action, Action::Move);
        assert_eq!(result[2].action, Action::Upsert);
    }

    #[test]
    fn upsert_state_converges_an_empty_directory_and_its_deletion() {
        let path = std::env::temp_dir().join(format!(
            "unearth-empty-dir-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let mut conn = live_entry_test_db();
        let mut caches = DbCaches::default();
        let tx = conn.transaction().unwrap();
        let state = upsert_path(
            &tx,
            &path,
            true,
            &Actor::reconcile(),
            EVENT_RECONCILE,
            &mut caches,
        )
        .unwrap();
        assert_eq!(state, IndexedPathState::Directory);
        tx.commit().unwrap();
        let indexed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries e
                 JOIN dirs d ON d.id=e.dir_id
                 JOIN strings s ON s.id=e.name_id
                 WHERE d.path || '/' || s.value=?1 AND e.kind=1",
                [normalize_index_dir(&path)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 1);

        fs::remove_dir(&path).unwrap();
        let tx = conn.transaction().unwrap();
        let state = upsert_path(
            &tx,
            &path,
            true,
            &Actor::reconcile(),
            EVENT_RECONCILE,
            &mut caches,
        )
        .unwrap();
        assert_eq!(state, IndexedPathState::Missing);
        tx.commit().unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn indexed_directory_move_rewrites_descendants_without_a_rescan() {
        let base = std::env::temp_dir().join(format!(
            "unearth-move-dir-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        let old = base.join("old");
        let child = old.join("child");
        let file = child.join("file.txt");
        fs::create_dir_all(&child).unwrap();
        fs::write(&file, b"test").unwrap();
        let mut conn = live_entry_test_db();
        let mut caches = DbCaches::default();
        {
            let tx = conn.transaction().unwrap();
            for (path, is_dir) in [(&old, true), (&child, true), (&file, false)] {
                upsert_path(
                    &tx,
                    path,
                    is_dir,
                    &Actor::unknown(),
                    EVENT_CREATE,
                    &mut caches,
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        let new = base.join("new");
        fs::rename(&old, &new).unwrap();
        let tx = conn.transaction().unwrap();
        assert!(move_indexed_directory(
            &tx,
            &old,
            &new,
            &Actor::unknown(),
            EVENT_MOVE,
            &mut caches,
        )
        .unwrap());
        tx.commit().unwrap();

        let old_key = normalize_index_dir(&old);
        let new_key = normalize_index_dir(&new);
        let old_prefix = index_path_prefix(&old_key);
        let new_prefix = index_path_prefix(&new_key);
        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dirs WHERE path=?1 OR path LIKE ?2",
                params![old_key, format!("{old_prefix}%")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);
        let new_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dirs WHERE path=?1 OR path LIKE ?2",
                params![new_key, format!("{new_prefix}%")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_count, 2);
        let file_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries e
                 JOIN dirs d ON d.id=e.dir_id
                 JOIN strings s ON s.id=e.name_id
                 WHERE d.path=?1 AND s.value='file.txt' AND e.kind=0",
                [normalize_index_dir(&new.join("child"))],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(file_count, 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn reconciliation_collapses_nested_paths_to_the_oldest_ancestor() {
        let paths = HashSet::from([
            PathBuf::from("/home/user/project"),
            PathBuf::from("/home/user/project/src"),
            PathBuf::from("/home/user/project/src/bin"),
            PathBuf::from("/home/user/other"),
        ]);
        let result = collapse_reconcile_paths(paths);
        assert_eq!(
            result,
            vec![
                PathBuf::from("/home/user/other"),
                PathBuf::from("/home/user/project")
            ]
        );
    }

    #[test]
    fn reconciliation_does_not_collapse_sibling_prefixes() {
        let paths = HashSet::from([
            PathBuf::from("/home/user/app"),
            PathBuf::from("/home/user/apple"),
        ]);
        let result = collapse_reconcile_paths(paths);
        assert_eq!(result.len(), 2);
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

    #[test]
    fn high_water_metric_only_moves_forward() {
        let value = AtomicU64::new(3);
        metric_max(Some(&value), 2);
        assert_eq!(value.load(Ordering::Relaxed), 3);
        metric_max(Some(&value), 9);
        assert_eq!(value.load(Ordering::Relaxed), 9);
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
