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
    let cli = cli::Cli::parse();
    if cli.build_info {
        fsx::build_info::print_json(fsx::build_info::current(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ));
        return;
    }
    if let Err(error) = app::run(cli) {
        if error.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("twig: {error}");
            std::process::exit(1);
        }
    }
}
