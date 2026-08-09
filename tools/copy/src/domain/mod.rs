//! Shared domain types passed between planning, transfer, telemetry, and UI.

use filetime::FileTime;
use rustc_hash::FxHashSet;
use std::path::PathBuf;
use std::sync::Arc;

mod eta;
mod progress;

pub(crate) use eta::{
    AtomicEtaProgress, EtaProgressTotals, EtaWorkSummary, EtaWorkload, ETA_BUCKET_S,
    ETA_FILE_BIN_COUNT, ETA_MAX_REGIME_HYPOTHESES, ETA_SAMPLE_RETENTION_S,
};
pub(crate) use progress::{
    DeviceIoDeltas, DeviceIoWindow, InflightWriteLimiter, InflightWritePermit, ProcIoCounters,
    ProcIoDeltas, ProcessIoWindow, ProgressSnapshot, TransferOutcome, TransferProgressRates,
    TransferRateSmoother,
};

pub(crate) enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferMode {
    Copy,
    Move,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollisionWinner {
    Source,
    Dest,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollisionCombineMode {
    Any,
    All,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CollisionPredicates {
    pub(crate) always: bool,
    pub(crate) newer: bool,
    pub(crate) larger: bool,
    pub(crate) size_differs: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct MergeCollisionPolicy {
    pub(crate) winner: CollisionWinner,
    pub(crate) combine: CollisionCombineMode,
    pub(crate) predicates: CollisionPredicates,
}

impl Default for MergeCollisionPolicy {
    fn default() -> Self {
        Self {
            winner: CollisionWinner::Source,
            combine: CollisionCombineMode::Any,
            predicates: CollisionPredicates {
                size_differs: true,
                ..CollisionPredicates::default()
            },
        }
    }
}

impl MergeCollisionPolicy {
    pub(crate) fn requires_mtime(self) -> bool {
        self.predicates.newer
    }
}

impl TransferMode {
    pub(crate) fn word(self) -> &'static str {
        match self {
            TransferMode::Copy => "copy",
            TransferMode::Move => "move",
        }
    }

    pub(crate) fn word_cap(self) -> &'static str {
        match self {
            TransferMode::Copy => "Copy",
            TransferMode::Move => "Move",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SrcObjKind {
    File,
    Dir,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DstObjKind {
    Dir,
    DirExisting,
    DirNew,
    File,
    FileExistingForDir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaKind {
    Nvme,
    Hdd,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferBackend {
    Rust,
    Rsync,
}

#[derive(Clone)]
pub(crate) struct RemoteSpec {
    pub(crate) user: Option<String>,
    pub(crate) host: String,
    pub(crate) path: String,
}

#[derive(Clone)]
pub(crate) enum Endpoint {
    Local(PathBuf),
    Remote(RemoteSpec),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    NewFile,
    ModFile,
    RemovedFile,
    NewDir,
    RemovedDir,
}

#[derive(Clone)]
pub(crate) struct ChangeItem {
    pub(crate) kind: ChangeKind,
    pub(crate) rel: String,
}

#[derive(Default, Clone)]
pub(crate) struct ManifestFileEntry {
    pub(crate) rel: Arc<str>,
    pub(crate) size: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    #[allow(dead_code)]
    pub(crate) nlink: u64,
    pub(crate) is_symlink: bool,
    pub(crate) mtime: Option<std::time::SystemTime>,
}

#[derive(Default, Clone)]
pub(crate) struct ManifestDeleteEntry {
    pub(crate) rel: Arc<str>,
    pub(crate) size: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) mtime: Option<std::time::SystemTime>,
    pub(crate) is_symlink: bool,
    pub(crate) link_target: Option<PathBuf>,
}

#[derive(Default, Clone)]
pub(crate) struct ManifestDeleteDirEntry {
    pub(crate) rel: String,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

#[derive(Clone)]
pub(crate) struct ManifestDirTimeEntry {
    pub(crate) rel: String,
    pub(crate) atime: FileTime,
    pub(crate) mtime: FileTime,
}

#[derive(Default, Clone)]
pub(crate) struct TransferManifest {
    pub(crate) dirs: Vec<String>,
    pub(crate) dir_times: Vec<ManifestDirTimeEntry>,
    pub(crate) copy_files: Vec<ManifestFileEntry>,
    pub(crate) identical_files: Vec<ManifestFileEntry>,
    pub(crate) sync_delete_files: Vec<ManifestDeleteEntry>,
    pub(crate) sync_delete_dirs: Vec<ManifestDeleteDirEntry>,
}

pub(crate) struct PreScan {
    pub(crate) scan_complete: bool,
    pub(crate) planned_bytes: u64,
    pub(crate) planned_bytes_exact: bool,
    pub(crate) total_regular_files: Option<u64>,
    pub(crate) total_regular_bytes: Option<u64>,
    pub(crate) total_dirs: Option<u64>,
    pub(crate) add_files: u64,
    pub(crate) mod_files: u64,
    pub(crate) uncollided_files: u64,
    pub(crate) add_dirs: u64,
    pub(crate) mod_dirs: u64,
    pub(crate) uncollided_dirs: u64,
    pub(crate) change_preview: Vec<ChangeItem>,
    pub(crate) source_display_paths: FxHashSet<String>,
    pub(crate) has_itemized_changes: bool,
    pub(crate) transfer_manifest: Option<TransferManifest>,
    pub(crate) file_relation_breakdown: FileRelationBreakdown,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct FileRelationBreakdown {
    pub(crate) same_time_same_size: u64,
    pub(crate) same_time_source_larger: u64,
    pub(crate) same_time_source_smaller: u64,
    pub(crate) same_size_source_newer: u64,
    pub(crate) same_size_source_older: u64,
    pub(crate) source_older_smaller: u64,
    pub(crate) source_older_larger: u64,
    pub(crate) source_newer_smaller: u64,
    pub(crate) source_newer_larger: u64,
}

impl FileRelationBreakdown {
    pub(crate) fn add_assign(&mut self, other: Self) {
        self.same_time_same_size = self
            .same_time_same_size
            .saturating_add(other.same_time_same_size);
        self.same_time_source_larger = self
            .same_time_source_larger
            .saturating_add(other.same_time_source_larger);
        self.same_time_source_smaller = self
            .same_time_source_smaller
            .saturating_add(other.same_time_source_smaller);
        self.same_size_source_newer = self
            .same_size_source_newer
            .saturating_add(other.same_size_source_newer);
        self.same_size_source_older = self
            .same_size_source_older
            .saturating_add(other.same_size_source_older);
        self.source_older_smaller = self
            .source_older_smaller
            .saturating_add(other.source_older_smaller);
        self.source_older_larger = self
            .source_older_larger
            .saturating_add(other.source_older_larger);
        self.source_newer_smaller = self
            .source_newer_smaller
            .saturating_add(other.source_newer_smaller);
        self.source_newer_larger = self
            .source_newer_larger
            .saturating_add(other.source_newer_larger);
    }
}

impl Default for PreScan {
    fn default() -> Self {
        Self {
            scan_complete: true,
            planned_bytes: 0,
            planned_bytes_exact: true,
            total_regular_files: None,
            total_regular_bytes: None,
            total_dirs: None,
            add_files: 0,
            mod_files: 0,
            uncollided_files: 0,
            add_dirs: 0,
            mod_dirs: 0,
            uncollided_dirs: 0,
            change_preview: Vec::new(),
            source_display_paths: FxHashSet::default(),
            has_itemized_changes: false,
            transfer_manifest: None,
            file_relation_breakdown: FileRelationBreakdown::default(),
        }
    }
}

pub(crate) struct CmdOutput {
    pub(crate) code: i32,
}

pub(crate) enum RsyncStreamEvent {
    Progress(u64),
    Text(String),
}

#[derive(Clone, Copy)]
pub(crate) struct DeleteCleanupOutcome {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    pub(crate) success: bool,
}

impl Default for DeleteCleanupOutcome {
    fn default() -> Self {
        Self {
            files: 0,
            bytes: 0,
            success: true,
        }
    }
}
