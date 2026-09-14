use crate::metadata::HardlinkKey;
use std::collections::HashSet;
use std::sync::Mutex;

/// Only multi-link candidates are retained. Unrelated identities need not
/// serialize on one process-wide lock during a parallel walk.
pub(super) struct HardlinkSet([Mutex<HashSet<HardlinkKey>>; 32]);

impl Default for HardlinkSet {
    fn default() -> Self {
        Self(std::array::from_fn(|_| Mutex::new(HashSet::new())))
    }
}

impl HardlinkSet {
    pub(super) fn insert(&self, key: HardlinkKey) -> bool {
        let mixed = key.inode.wrapping_mul(0x9e3779b97f4a7c15) ^ key.device.rotate_left(17);
        self.0[(mixed ^ (mixed >> 32)) as usize % self.0.len()]
            .lock()
            .map(|mut shard| shard.insert(key))
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_candidates_are_counted_exactly_once() {
        let seen = HardlinkSet::default();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        (0..4096)
                            .filter(|&inode| seen.insert(HardlinkKey { device: 3, inode }))
                            .count()
                    })
                })
                .collect();
            assert_eq!(
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .sum::<usize>(),
                4096
            );
        });
    }
}
