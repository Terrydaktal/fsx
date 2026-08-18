mod app;

use std::process::ExitCode;

#[cfg(feature = "index")]
fn install_crash_capture(package: &'static str, version: &'static str) {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::Once;
    use std::time::{SystemTime, UNIX_EPOCH};

    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_secs())
                .unwrap_or(0);
            if let Some(cache) = fsx::index::cache_dir() {
                let directory = cache.join("crashes");
                if fs::create_dir_all(&directory).is_ok() {
                    let path = directory.join(format!(
                        "fsxd-panic-{}-{timestamp}.json",
                        std::process::id()
                    ));
                    if let Ok(mut file) = OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .mode(0o600)
                        .open(path)
                    {
                        let payload = panic
                            .payload()
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| {
                                panic.payload().downcast_ref::<String>().map(String::as_str)
                            })
                            .unwrap_or("panic");
                        let location = panic
                            .location()
                            .map(|value| format!("{}:{}", value.file(), value.line()))
                            .unwrap_or_else(|| "unknown".to_string());
                        let _ = writeln!(
                            file,
                            "{}",
                            crash_record_json(
                                package,
                                version,
                                std::process::id(),
                                timestamp,
                                payload,
                                &location,
                            )
                        );
                    }
                }
            }
            previous(panic);
        }));
    });
}

#[cfg(feature = "index")]
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(feature = "index")]
fn crash_record_json(
    package: &str,
    version: &str,
    pid: u32,
    timestamp: u64,
    message: &str,
    location: &str,
) -> String {
    format!(
        "{{\"schema\":1,\"package\":\"{}\",\"version\":\"{}\",\"pid\":{},\"timestamp\":{},\"message\":\"{}\",\"location\":\"{}\"}}",
        escape(package),
        escape(version),
        pid,
        timestamp,
        escape(message),
        escape(location),
    )
}

pub fn run_cli() -> ExitCode {
    app::cli_main()
}

pub fn run_fsxd() -> ExitCode {
    app::fsxd_main(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

pub fn run_fsxd_with_identity(package: &'static str, version: &'static str) -> ExitCode {
    #[cfg(feature = "index")]
    install_crash_capture(package, version);
    app::fsxd_main(package, version)
}

#[cfg(all(test, feature = "index"))]
mod tests {
    use super::crash_record_json;

    #[test]
    fn panic_marker_is_structured_and_escaped() {
        let marker = crash_record_json("fsxd", "1", 42, 7, "bad \" input", "src/lib.rs:9");
        assert!(marker.contains("\"schema\":1"));
        assert!(marker.contains("bad \\\" input"));
        assert!(marker.contains("\"pid\":42"));
    }
}
