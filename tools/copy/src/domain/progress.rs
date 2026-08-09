use super::{EtaProgressTotals, EtaWorkload};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub(crate) struct TransferOutcome {
    pub(crate) rc: i32,
    pub(crate) bytes_done: u64,
    pub(crate) elapsed_s: f64,
    pub(crate) progress_snapshot: Option<ProgressSnapshot>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TransferProgressRates {
    pub(crate) write_all_bps: Option<f64>,
    pub(crate) rchar_bps: Option<f64>,
    pub(crate) wchar_bps: Option<f64>,
    pub(crate) read_bytes_bps: Option<f64>,
    pub(crate) write_bytes_bps: Option<f64>,
    pub(crate) read_complete_bps: Option<f64>,
    pub(crate) write_complete_bps: Option<f64>,
}

#[derive(Default)]
pub(crate) struct TransferRateSmoother {
    pub(crate) rates: TransferProgressRates,
}

impl TransferRateSmoother {
    pub(crate) fn update(
        &mut self,
        sample: TransferProgressRates,
        dt: f64,
    ) -> TransferProgressRates {
        let alpha = (1.0 - (-dt.max(0.0) / 1.25).exp()).clamp(0.05, 1.0);
        fn blend(previous: Option<f64>, current: Option<f64>, alpha: f64) -> Option<f64> {
            match (previous, current) {
                (Some(previous), Some(current)) => Some(previous + alpha * (current - previous)),
                (None, current) => current,
                (previous, None) => previous,
            }
        }
        self.rates.write_all_bps = blend(self.rates.write_all_bps, sample.write_all_bps, alpha);
        self.rates.rchar_bps = blend(self.rates.rchar_bps, sample.rchar_bps, alpha);
        self.rates.wchar_bps = blend(self.rates.wchar_bps, sample.wchar_bps, alpha);
        self.rates.read_bytes_bps = blend(self.rates.read_bytes_bps, sample.read_bytes_bps, alpha);
        self.rates.write_bytes_bps =
            blend(self.rates.write_bytes_bps, sample.write_bytes_bps, alpha);
        self.rates.read_complete_bps = blend(
            self.rates.read_complete_bps,
            sample.read_complete_bps,
            alpha,
        );
        self.rates.write_complete_bps = blend(
            self.rates.write_complete_bps,
            sample.write_complete_bps,
            alpha,
        );
        self.rates
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ProcIoCounters {
    pub(crate) rchar: u64,
    pub(crate) wchar: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ProcIoDeltas {
    pub(crate) rchar: Option<u64>,
    pub(crate) wchar: Option<u64>,
    pub(crate) read_bytes: Option<u64>,
    pub(crate) write_bytes: Option<u64>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct DeviceIoDeltas {
    pub(crate) read_complete: Option<u64>,
    pub(crate) write_complete: Option<u64>,
}

#[derive(Clone, Default)]
pub(crate) struct ProgressSnapshot {
    pub(crate) elapsed_s: f64,
    pub(crate) planned_bytes: u64,
    pub(crate) write_all_total: Option<u64>,
    pub(crate) phase_label: &'static str,
    pub(crate) rates: TransferProgressRates,
    pub(crate) proc_deltas: ProcIoDeltas,
    pub(crate) device_deltas: DeviceIoDeltas,
    pub(crate) eta_workload: Option<EtaWorkload>,
    pub(crate) eta_progress: Option<EtaProgressTotals>,
}

#[derive(Default)]
pub(crate) struct DeviceIoWindow {
    pub(crate) src_keys: Vec<(u64, u64)>,
    pub(crate) dst_keys: Vec<(u64, u64)>,
}

#[derive(Default)]
pub(crate) struct ProcessIoWindow {
    pub(crate) pid: u32,
    pub(crate) last_at: Option<Instant>,
    pub(crate) last_counters: Option<ProcIoCounters>,
}

pub(crate) struct InflightWriteLimiter {
    pub(crate) max_bytes: u64,
    pub(crate) used_bytes: Mutex<u64>,
    pub(crate) cv: Condvar,
}

pub(crate) struct InflightWritePermit {
    pub(crate) limiter: Arc<InflightWriteLimiter>,
    pub(crate) reserved: u64,
}

impl Drop for InflightWritePermit {
    fn drop(&mut self) {
        self.limiter.release(self.reserved);
    }
}

impl InflightWriteLimiter {
    pub(crate) fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes: max_bytes.max(1),
            used_bytes: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn acquire(self: &Arc<Self>, want_bytes: u64) -> InflightWritePermit {
        let reserve = want_bytes.max(1).min(self.max_bytes);
        let mut used = self.used_bytes.lock().unwrap_or_else(|e| e.into_inner());
        while (*used + reserve > self.max_bytes) && *used > 0 {
            used = self.cv.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used = used.saturating_add(reserve);
        drop(used);
        InflightWritePermit {
            limiter: Arc::clone(self),
            reserved: reserve,
        }
    }

    pub(crate) fn release(&self, bytes: u64) {
        let mut used = self.used_bytes.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(bytes);
        self.cv.notify_all();
    }
}
