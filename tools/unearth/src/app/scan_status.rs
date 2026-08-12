use std::sync::atomic::{AtomicU64, Ordering};

static LIVE_SCAN_ERRORS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn reset_live_scan_errors() {
    LIVE_SCAN_ERRORS.store(0, Ordering::Relaxed);
}

pub(crate) fn live_scan_incomplete() -> bool {
    LIVE_SCAN_ERRORS.load(Ordering::Relaxed) != 0
}

pub(super) fn record_scan_error() {
    LIVE_SCAN_ERRORS.fetch_add(1, Ordering::Relaxed);
}
