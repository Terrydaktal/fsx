//! Account for snapshots across the queue, batches, and pending reconciliation.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub(super) struct SnapshotBudget {
    used: AtomicUsize,
    limit: usize,
}

#[derive(Debug)]
pub(super) struct SnapshotLease {
    bytes: usize,
    budget: Arc<SnapshotBudget>,
}

impl SnapshotBudget {
    pub(super) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
        })
    }

    pub(super) fn reserve(self: &Arc<Self>, bytes: usize) -> Option<Arc<SnapshotLease>> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.limit)
            })
            .ok()?;
        Some(Arc::new(SnapshotLease {
            bytes,
            budget: Arc::clone(self),
        }))
    }
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_held_until_last_snapshot_reference_is_released() {
        let budget = SnapshotBudget::new(128);
        let lease = budget.reserve(100).unwrap();
        let copy = Arc::clone(&lease);
        assert!(budget.reserve(29).is_none());
        drop(lease);
        assert!(budget.reserve(29).is_none());
        drop(copy);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        assert!(budget.reserve(128).is_some());
        assert!(budget.reserve(usize::MAX).is_none());
    }
}
