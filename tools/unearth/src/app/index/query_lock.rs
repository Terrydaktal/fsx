use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Serialize query-socket startup and reclaim only locks whose owner is gone.
pub(super) fn acquire_query_start_lock(lock: &Path, socket: &Path) -> Result<File, String> {
    for _ in 0..3 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(lock)
        {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id()).map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
                return Ok(file);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if UnixStream::connect(socket).is_ok() {
                    return Err("another fsxd query server is already running".to_string());
                }
                let observed = fs::read_to_string(lock).unwrap_or_default();
                let pid = observed.trim().parse::<u32>().ok();
                if pid.is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists()) {
                    return Err("another fsxd query server is starting".to_string());
                }
                if fs::read_to_string(lock).unwrap_or_default() != observed {
                    continue;
                }
                let _ = fs::remove_file(lock);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("query server startup lock is busy".to_string())
}
