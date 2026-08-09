use clap::Parser;
use jemallocator::Jemalloc;
use std::io;

mod app;
mod cli;
mod fs_ops;
mod git;
mod model;
mod render;

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
