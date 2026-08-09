mod app;

use std::process::ExitCode;

pub fn run_cli() -> ExitCode {
    app::cli_main()
}

pub fn run_fsxd() -> ExitCode {
    app::fsxd_main()
}
