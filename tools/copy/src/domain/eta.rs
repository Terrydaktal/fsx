use super::{MediaKind, TransferManifest};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub(crate) const ETA_FILE_BIN_COUNT: usize = 8;
pub(crate) const ETA_MAX_REGIME_HYPOTHESES: usize = 64;
pub(crate) const ETA_BUCKET_S: f64 = 1.0;
pub(crate) const ETA_SAMPLE_RETENTION_S: f64 = 90.0;
pub(crate) const ETA_OPERATION_SEGMENT_SIZE: usize = 256;

pub(crate) type EtaFileCounts = [u64; ETA_FILE_BIN_COUNT];
pub(crate) type EtaFileBytes = [u64; ETA_FILE_BIN_COUNT];

pub(crate) fn eta_file_bin(size: u64) -> usize {
    if size <= 4 * 1024 {
        0
    } else if size <= 16 * 1024 {
        1
    } else if size <= 64 * 1024 {
        2
    } else if size <= 256 * 1024 {
        3
    } else if size <= 1024 * 1024 {
        4
    } else if size <= 4 * 1024 * 1024 {
        5
    } else if size <= 64 * 1024 * 1024 {
        6
    } else {
        7
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct EtaWorkOp {
    pub(crate) bytes: u64,
    pub(crate) bin: u8,
    pub(crate) is_dir: bool,
    pub(crate) is_metadata: bool,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct EtaWorkSummary {
    pub(crate) file_bins: EtaFileCounts,
    pub(crate) file_bytes: EtaFileBytes,
    pub(crate) dirs: u64,
    pub(crate) metadata: u64,
}

struct EtaAtomicSummary {
    file_bins: [AtomicU64; ETA_FILE_BIN_COUNT],
    file_bytes: [AtomicU64; ETA_FILE_BIN_COUNT],
    dirs: AtomicU64,
    metadata: AtomicU64,
}

impl EtaAtomicSummary {
    fn new(summary: EtaWorkSummary) -> Self {
        Self {
            file_bins: std::array::from_fn(|idx| AtomicU64::new(summary.file_bins[idx])),
            file_bytes: std::array::from_fn(|idx| AtomicU64::new(summary.file_bytes[idx])),
            dirs: AtomicU64::new(summary.dirs),
            metadata: AtomicU64::new(summary.metadata),
        }
    }

    fn snapshot(&self) -> EtaWorkSummary {
        EtaWorkSummary {
            file_bins: std::array::from_fn(|idx| self.file_bins[idx].load(Ordering::Relaxed)),
            file_bytes: std::array::from_fn(|idx| self.file_bytes[idx].load(Ordering::Relaxed)),
            dirs: self.dirs.load(Ordering::Relaxed),
            metadata: self.metadata.load(Ordering::Relaxed),
        }
    }
}

impl EtaWorkSummary {
    pub(crate) fn add_op(&mut self, op: EtaWorkOp) {
        if op.is_metadata {
            self.metadata = self.metadata.saturating_add(1);
        } else if op.is_dir {
            self.dirs = self.dirs.saturating_add(1);
            return;
        }
        let bin = usize::from(op.bin).min(ETA_FILE_BIN_COUNT - 1);
        self.file_bins[bin] = self.file_bins[bin].saturating_add(1);
        self.file_bytes[bin] = self.file_bytes[bin].saturating_add(op.bytes);
    }

    pub(crate) fn add_assign(&mut self, other: Self) {
        for idx in 0..ETA_FILE_BIN_COUNT {
            self.file_bins[idx] = self.file_bins[idx].saturating_add(other.file_bins[idx]);
            self.file_bytes[idx] = self.file_bytes[idx].saturating_add(other.file_bytes[idx]);
        }
        self.dirs = self.dirs.saturating_add(other.dirs);
        self.metadata = self.metadata.saturating_add(other.metadata);
    }
}

struct EtaOrderedPlan {
    operations: Arc<[EtaWorkOp]>,
    segments: Arc<[EtaWorkSegment]>,
    completed: Arc<[std::sync::atomic::AtomicBool]>,
}

struct EtaWorkSegment {
    remaining: EtaAtomicSummary,
}

#[derive(Clone)]
pub(crate) struct EtaWorkload {
    pub(crate) file_bins: EtaFileCounts,
    pub(crate) file_bytes: EtaFileBytes,
    pub(crate) dirs: u64,
    pub(crate) metadata: u64,
    pub(crate) media: MediaKind,
    pub(crate) profile_key: u64,
    ordered: Arc<EtaOrderedPlan>,
}

impl Default for EtaWorkload {
    fn default() -> Self {
        Self::from_ops(Vec::new(), MediaKind::Other, 0)
    }
}

impl EtaWorkload {
    fn from_ops(operations: Vec<EtaWorkOp>, media: MediaKind, profile_key: u64) -> Self {
        let mut segments = Vec::new();
        for chunk in operations.chunks(ETA_OPERATION_SEGMENT_SIZE) {
            let mut summary = EtaWorkSummary::default();
            for op in chunk.iter().copied() {
                summary.add_op(op);
            }
            segments.push(EtaWorkSegment {
                remaining: EtaAtomicSummary::new(summary),
            });
        }

        let mut total = EtaWorkSummary::default();
        for segment in &segments {
            total.add_assign(segment.remaining.snapshot());
        }

        let operation_count = operations.len();
        let ordered = Arc::new(EtaOrderedPlan {
            operations: Arc::from(operations.into_boxed_slice()),
            segments: Arc::from(segments.into_boxed_slice()),
            completed: Arc::from(
                std::iter::repeat_with(|| std::sync::atomic::AtomicBool::new(false))
                    .take(operation_count)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
        });
        Self {
            file_bins: total.file_bins,
            file_bytes: total.file_bytes,
            dirs: total.dirs,
            metadata: total.metadata,
            media,
            profile_key,
            ordered,
        }
    }

    pub(crate) fn from_manifest(
        manifest: &TransferManifest,
        include_root: bool,
        media: MediaKind,
        profile_key: u64,
    ) -> Self {
        let mut operations = Vec::with_capacity(
            manifest
                .dirs
                .len()
                .saturating_add(manifest.copy_files.len())
                .saturating_add(manifest.dir_times.len())
                .saturating_add(usize::from(include_root)),
        );
        if include_root {
            operations.push(EtaWorkOp {
                is_dir: true,
                ..EtaWorkOp::default()
            });
        }
        operations.extend(manifest.dirs.iter().map(|_| EtaWorkOp {
            is_dir: true,
            ..EtaWorkOp::default()
        }));
        operations.extend(manifest.copy_files.iter().map(|entry| EtaWorkOp {
            bytes: entry.size,
            bin: eta_file_bin(entry.size) as u8,
            is_dir: false,
            is_metadata: false,
        }));
        operations.extend(manifest.dir_times.iter().map(|_| EtaWorkOp {
            is_metadata: true,
            ..EtaWorkOp::default()
        }));
        Self::from_ops(operations, media, profile_key)
    }

    pub(crate) fn from_file(size: u64, media: MediaKind, profile_key: u64) -> Self {
        Self::from_ops(
            vec![EtaWorkOp {
                bytes: size,
                bin: eta_file_bin(size) as u8,
                is_dir: false,
                is_metadata: false,
            }],
            media,
            profile_key,
        )
    }

    pub(crate) fn from_file_sizes(sizes: &[u64], media: MediaKind, profile_key: u64) -> Self {
        Self::from_ops(
            sizes
                .iter()
                .copied()
                .map(|size| EtaWorkOp {
                    bytes: size,
                    bin: eta_file_bin(size) as u8,
                    is_dir: false,
                    is_metadata: false,
                })
                .collect(),
            media,
            profile_key,
        )
    }

    pub(crate) fn remaining_ordered(&self, progress: EtaProgressTotals) -> EtaWorkSummary {
        let mut remaining = EtaWorkSummary::default();
        self.for_each_remaining_segment(progress, |segment| remaining.add_assign(segment));
        remaining
    }

    pub(crate) fn mark_operation(&self, operation_index: usize) {
        let Some(completed) = self.ordered.completed.get(operation_index) else {
            return;
        };
        if completed
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let segment_index = operation_index / ETA_OPERATION_SEGMENT_SIZE;
        let op = self.ordered.operations[operation_index];
        let remaining = &self.ordered.segments[segment_index].remaining;
        if op.is_metadata {
            remaining.metadata.fetch_sub(1, Ordering::Relaxed);
        } else if op.is_dir {
            remaining.dirs.fetch_sub(1, Ordering::Relaxed);
        } else {
            let bin = usize::from(op.bin).min(ETA_FILE_BIN_COUNT - 1);
            remaining.file_bins[bin].fetch_sub(1, Ordering::Relaxed);
            remaining.file_bytes[bin].fetch_sub(op.bytes, Ordering::Relaxed);
        }
    }

    pub(crate) fn for_each_remaining_segment(
        &self,
        progress: EtaProgressTotals,
        mut visit: impl FnMut(EtaWorkSummary),
    ) {
        if self.ordered.operations.is_empty() {
            visit(EtaWorkSummary {
                file_bins: std::array::from_fn(|idx| {
                    self.file_bins[idx].saturating_sub(progress.file_bins[idx])
                }),
                file_bytes: std::array::from_fn(|idx| {
                    self.file_bytes[idx].saturating_sub(progress.file_bytes[idx])
                }),
                dirs: self.dirs.saturating_sub(progress.dirs),
                metadata: self.metadata,
            });
            return;
        }

        for segment in self.ordered.segments.iter() {
            let remaining = segment.remaining.snapshot();
            if remaining.file_bins.iter().any(|count| *count > 0)
                || remaining.dirs > 0
                || remaining.metadata > 0
                || remaining.file_bytes.iter().any(|bytes| *bytes > 0)
            {
                visit(remaining);
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct EtaProgressTotals {
    pub(crate) file_bins: EtaFileCounts,
    pub(crate) file_bytes: EtaFileBytes,
    pub(crate) dirs: u64,
    pub(crate) metadata: u64,
}

impl EtaProgressTotals {
    pub(crate) fn file_count(self) -> u64 {
        self.file_bins.iter().copied().sum()
    }

    pub(crate) fn delta(self, previous: Self) -> Self {
        Self {
            file_bins: std::array::from_fn(|idx| {
                self.file_bins[idx].saturating_sub(previous.file_bins[idx])
            }),
            file_bytes: std::array::from_fn(|idx| {
                self.file_bytes[idx].saturating_sub(previous.file_bytes[idx])
            }),
            dirs: self.dirs.saturating_sub(previous.dirs),
            metadata: self.metadata.saturating_sub(previous.metadata),
        }
    }
}

pub(crate) struct AtomicEtaProgress {
    pub(crate) file_bins: [AtomicU64; ETA_FILE_BIN_COUNT],
    pub(crate) file_bytes: [AtomicU64; ETA_FILE_BIN_COUNT],
    pub(crate) dirs: AtomicU64,
    pub(crate) metadata: AtomicU64,
}

impl Default for AtomicEtaProgress {
    fn default() -> Self {
        Self {
            file_bins: std::array::from_fn(|_| AtomicU64::new(0)),
            file_bytes: std::array::from_fn(|_| AtomicU64::new(0)),
            dirs: AtomicU64::new(0),
            metadata: AtomicU64::new(0),
        }
    }
}

impl AtomicEtaProgress {
    pub(crate) fn mark_file(&self, size: u64) {
        let bin = eta_file_bin(size);
        self.file_bins[bin].fetch_add(1, Ordering::Relaxed);
        self.file_bytes[bin].fetch_add(size, Ordering::Relaxed);
    }

    pub(crate) fn mark_dir(&self) {
        self.dirs.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn mark_metadata(&self, count: u64) {
        self.metadata.fetch_add(count, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> EtaProgressTotals {
        EtaProgressTotals {
            file_bins: std::array::from_fn(|idx| self.file_bins[idx].load(Ordering::Relaxed)),
            file_bytes: std::array::from_fn(|idx| self.file_bytes[idx].load(Ordering::Relaxed)),
            dirs: self.dirs.load(Ordering::Relaxed),
            metadata: self.metadata.load(Ordering::Relaxed),
        }
    }
}
