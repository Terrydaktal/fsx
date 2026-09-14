//! Reuse one bounded scan pool for traversal and metadata population.
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::sync::{Arc, Mutex, OnceLock};

type CachedPool = Mutex<Option<(usize, Arc<ThreadPool>)>>;

pub(super) fn pool(threads: usize) -> Option<Arc<ThreadPool>> {
    if threads <= 1 {
        return None;
    }
    static CACHE: OnceLock<CachedPool> = OnceLock::new();
    let mut cached = CACHE.get_or_init(|| Mutex::new(None)).lock().ok()?;
    if let Some((size, pool)) = cached.as_ref() {
        if *size == threads {
            return Some(Arc::clone(pool));
        }
    }
    let pool = Arc::new(
        ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|id| format!("fsxd-scan-{id}"))
            .build()
            .ok()?,
    );
    *cached = Some((threads, Arc::clone(&pool)));
    Some(pool)
}

pub(super) fn parallelism(threads: usize) -> jwalk::Parallelism {
    match pool(threads) {
        // A nested walk must not wait for workers occupied by its own caller.
        Some(pool) if pool.current_thread_index().is_none() => {
            jwalk::Parallelism::RayonExistingPool {
                pool,
                busy_timeout: None,
            }
        }
        _ => jwalk::Parallelism::Serial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_scans_reuse_workers_and_serial_does_not_create_a_pool() {
        assert!(pool(1).is_none());
        let first = pool(3).unwrap();
        let second = pool(3).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.current_num_threads(), 3);
        assert!(first.install(|| matches!(parallelism(3), jwalk::Parallelism::Serial)));
    }
}
