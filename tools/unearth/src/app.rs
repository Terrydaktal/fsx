use std::time::Duration;

#[cfg(feature = "watcher")]
mod watcher;

mod aggregate;
mod cli;
mod filesystem;
#[cfg(feature = "index")]
mod index;
mod model;
mod patterns;
mod presentation;
mod scan_status;
mod search;

pub(crate) use cli::{cli_main, fsxd_main};
#[allow(unused_imports)]
pub(crate) use filesystem::*;
#[cfg(feature = "index")]
#[allow(unused_imports)]
pub(crate) use index::*;
#[allow(unused_imports)]
pub(crate) use model::*;
#[allow(unused_imports)]
pub(crate) use patterns::*;
#[allow(unused_imports)]
pub(crate) use presentation::*;
pub(crate) use scan_status::LiveScanStatus;

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
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
#[cfg(feature = "index")]
const SNAPSHOT_REFRESH_MIN_AGE: Duration = Duration::from_secs(30);
#[cfg(feature = "index")]
const INDEX_REFRESH_MIN_AGE: Duration = Duration::from_secs(30);
#[cfg(feature = "index")]
const INDEX_STREAM_FLUSH_LINES: usize = 1_000;
#[cfg(feature = "index")]
const INDEX_INSERT_BATCH_SIZE: usize = 1_000;
#[cfg(feature = "index")]
const INDEX_INCREMENTAL_MAX_CHANGES: usize = 100_000;
#[cfg(feature = "index")]
const INDEX_INCREMENTAL_CHANGE_DIVISOR: usize = 5;
#[cfg(feature = "index")]
const INDEX_REFRESH_LOCK_EMPTY_GRACE: Duration = Duration::from_secs(10);
#[cfg(feature = "index")]
const INDEX_SNAPSHOT_MAGIC: &[u8; 8] = b"UNRTHS01";
#[cfg(feature = "index")]
const INDEX_MANIFEST_MAGIC: &[u8; 8] = b"UNRMNF04";
#[cfg(feature = "index")]
const INDEX_DELTA_MAGIC: &[u8; 8] = b"UNRDLT01";
#[cfg(feature = "index")]
const INDEX_DELTA_COMPACT_RECORDS: usize = 50_000;
#[cfg(feature = "index")]
const QUERY_SOCKET_NAME: &str = "fsxd.sock";
#[cfg(feature = "index")]
const QUERY_PROTOCOL_MAGIC: &[u8; 8] = b"FSXQ0001";
#[cfg(feature = "index")]
const QUERY_RESPONSE_MAGIC: &[u8; 8] = b"FSXS0001";
#[cfg(feature = "index")]
const QUERY_MAX_FRAME: usize = 16 * 1024 * 1024;
#[cfg(feature = "watcher")]
const QUERY_MAX_REQUEST: usize = 64 * 1024 * 1024;
#[cfg(feature = "index")]
const INDEX_SCHEMA_VERSION: i32 = 4;

#[cfg(all(test, feature = "index"))]
mod tests;
