use std::time::Duration;

#[cfg(feature = "watcher")]
mod watcher;

mod cli;
mod filesystem;
mod index;
mod model;
mod patterns;
mod presentation;
mod search;

pub(crate) use cli::{cli_main, fsxd_main};
pub(crate) use filesystem::*;
#[cfg(any(feature = "watcher", test))]
pub(crate) use index::*;
pub(crate) use model::*;
pub(crate) use patterns::*;
pub(crate) use presentation::*;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[cfg(all(feature = "watcher", not(target_env = "msvc")))]
fn purge_unused_allocator_pages() {
    let name = b"arena.4096.purge\0";
    unsafe {
        jemalloc_sys::mallctl(
            name.as_ptr().cast(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        );
    }
}

#[cfg(all(feature = "watcher", target_env = "msvc"))]
fn purge_unused_allocator_pages() {}

const VERSION: &str = env!("CARGO_PKG_VERSION");
const NTFS_FS_TYPES: [&str; 3] = ["ntfs", "ntfs3", "fuseblk"];
const ROOT_SIZE_SKIP_TREES: [&str; 6] = ["/mnt", "/media", "/dev", "/proc", "/sys", "/run"];
const SNAPSHOT_REFRESH_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const SNAPSHOT_REFRESH_MIN_AGE: Duration = Duration::from_secs(30);
const INDEX_REFRESH_MIN_AGE: Duration = Duration::from_secs(30);
const INDEX_STREAM_FLUSH_LINES: usize = 1_000;
const INDEX_INSERT_BATCH_SIZE: usize = 1_000;
const INDEX_INCREMENTAL_MAX_CHANGES: usize = 100_000;
const INDEX_INCREMENTAL_CHANGE_DIVISOR: usize = 5;
const INDEX_REFRESH_LOCK_EMPTY_GRACE: Duration = Duration::from_secs(10);
const INDEX_SNAPSHOT_MAGIC: &[u8; 8] = b"UNRTHS01";
const INDEX_MANIFEST_MAGIC: &[u8; 8] = b"UNRMNF04";
const INDEX_DELTA_MAGIC: &[u8; 8] = b"UNRDLT01";
const INDEX_DELTA_COMPACT_RECORDS: usize = 50_000;
const QUERY_SOCKET_NAME: &str = "fsxd.sock";
const QUERY_PROTOCOL_MAGIC: &[u8; 8] = b"FSXQ0001";
const QUERY_RESPONSE_MAGIC: &[u8; 8] = b"FSXS0001";
const QUERY_MAX_FRAME: usize = 16 * 1024 * 1024;
#[cfg(feature = "watcher")]
const QUERY_MAX_REQUEST: usize = 64 * 1024 * 1024;
const INDEX_SCHEMA_VERSION: i32 = 4;

#[cfg(test)]
mod tests;
