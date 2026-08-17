use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Completion state for one live filesystem search.
///
/// Walk workers clone this handle, while concurrent searches create separate
/// handles. This prevents one search from clearing or inheriting another
/// search's errors.
#[derive(Clone, Default)]
pub(crate) struct LiveScanStatus {
    incomplete: Arc<AtomicBool>,
}

impl LiveScanStatus {
    pub(crate) fn is_incomplete(&self) -> bool {
        self.incomplete.load(Ordering::Acquire)
    }

    pub(super) fn record_error(&self) {
        self.incomplete.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::LiveScanStatus;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_scan_statuses_are_isolated() {
        let errored = LiveScanStatus::default();
        let clean = LiveScanStatus::default();
        let start = Arc::new(Barrier::new(3));
        let recorded = Arc::new(Barrier::new(3));

        let error_worker = {
            let status = errored.clone();
            let start = Arc::clone(&start);
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || {
                start.wait();
                status.record_error();
                recorded.wait();
                assert!(status.is_incomplete());
            })
        };
        let clean_worker = {
            let status = clean.clone();
            let start = Arc::clone(&start);
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || {
                start.wait();
                recorded.wait();
                assert!(!status.is_incomplete());
            })
        };

        start.wait();
        recorded.wait();
        error_worker.join().unwrap();
        clean_worker.join().unwrap();

        assert!(errored.is_incomplete());
        assert!(!clean.is_incomplete());
    }
}
