use clap::Parser;
#[cfg(feature = "jemalloc")]
use jemallocator::Jemalloc;
use std::io;

mod app;
mod cli;
mod fs_ops;
mod git;
mod model;
mod render;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() {
    if let Err(error) = app::run(cli::Cli::parse()) {
        if error.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("twig: {error}");
            std::process::exit(1);
        }
    }
}
