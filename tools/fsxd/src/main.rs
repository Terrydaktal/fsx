use std::process::ExitCode;

fn main() -> ExitCode {
    unearth::run_fsxd_with_identity(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
