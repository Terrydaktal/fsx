//! Library entry point for the `copy-rs` command and its internal modules.

mod app;
mod cli;
mod domain;
mod output;
mod plan;
mod runtime;
mod transfer;

#[cfg(feature = "jemalloc")]
use jemallocator::Jemalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

pub fn run() -> i32 {
    app::run()
}
