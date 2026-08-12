use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

pub fn write_raw_paths(dirs: &[PathBuf], files: &[PathBuf]) -> io::Result<()> {
    let mut cache = RawPathCache::open()?;
    for path in dirs {
        cache.write_dir(path)?;
    }
    for path in files {
        cache.write_file(path)?;
    }
    cache.finish()
}

pub struct RawPathCache {
    dirs: BufWriter<File>,
    files: BufWriter<File>,
    dirs_temp: PathBuf,
    files_temp: PathBuf,
    dirs_final: PathBuf,
    files_final: PathBuf,
    lock_path: PathBuf,
    _lock: File,
}

impl RawPathCache {
    pub fn open() -> io::Result<Self> {
        let cache_dir = cache_directory()?;
        let pid = fish_pid_suffix();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dirs_final = cache_dir.join(format!("universal-last-dirs-{pid}"));
        let files_final = cache_dir.join(format!("universal-last-files-{pid}"));
        let lock_path = cache_dir.join(format!(".universal-last-{pid}.lock"));
        let dirs_temp = cache_dir.join(format!(".universal-last-dirs-{pid}.{nonce}.tmp"));
        let files_temp = cache_dir.join(format!(".universal-last-files-{pid}.{nonce}.tmp"));
        let lock = acquire_cache_lock(&lock_path)?;
        let dirs_file = match open_secure_file(&dirs_temp) {
            Ok(file) => file,
            Err(error) => {
                let _ = std::fs::remove_file(&lock_path);
                return Err(error);
            }
        };
        let files_file = match open_secure_file(&files_temp) {
            Ok(file) => file,
            Err(error) => {
                let _ = std::fs::remove_file(&dirs_temp);
                let _ = std::fs::remove_file(&lock_path);
                return Err(error);
            }
        };
        Ok(Self {
            dirs: BufWriter::new(dirs_file),
            files: BufWriter::new(files_file),
            dirs_temp,
            files_temp,
            dirs_final,
            files_final,
            lock_path,
            _lock: lock,
        })
    }

    pub fn write_dir(&mut self, path: &Path) -> io::Result<()> {
        write_path(&mut self.dirs, path)
    }

    pub fn write_file(&mut self, path: &Path) -> io::Result<()> {
        write_path(&mut self.files, path)
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.publish()
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.publish()
    }

    fn publish(&mut self) -> io::Result<()> {
        self.dirs.flush()?;
        self.files.flush()?;
        self.dirs.get_ref().sync_all()?;
        self.files.get_ref().sync_all()?;
        std::fs::rename(&self.dirs_temp, &self.dirs_final)?;
        std::fs::rename(&self.files_temp, &self.files_final)?;
        if let Some(parent) = self.dirs_final.parent() {
            File::open(parent)?.sync_all()?;
        }
        self.dirs_temp = next_temp_path(&self.dirs_final)?;
        self.files_temp = next_temp_path(&self.files_final)?;
        self.dirs = BufWriter::new(open_secure_file(&self.dirs_temp)?);
        self.files = BufWriter::new(open_secure_file(&self.files_temp)?);
        Ok(())
    }
}

impl Drop for RawPathCache {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.dirs_temp);
        let _ = std::fs::remove_file(&self.files_temp);
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

fn next_temp_path(final_path: &Path) -> io::Result<PathBuf> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(final_path.with_file_name(format!(
        ".{}.{}.tmp",
        final_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("paths"),
        nonce
    )))
}

fn write_path(writer: &mut BufWriter<File>, path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        if bytes.iter().any(|byte| *byte == b'\n' || *byte == b'\r') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "raw path cache cannot represent newline-containing paths",
            ));
        }
        writer.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    {
        let text = path.to_string_lossy();
        if text.contains(['\n', '\r']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "raw path cache cannot represent newline-containing paths",
            ));
        }
        writer.write_all(text.as_bytes())?;
    }
    writer.write_all(b"\n")
}

fn cache_directory() -> io::Result<PathBuf> {
    let user = std::env::var_os("USER")
        .and_then(|value| value.into_string().ok())
        .map(|value| {
            value
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let cache_dir = Path::new("/tmp").join(format!("fzf-history-{user}"));
    match std::fs::symlink_metadata(&cache_dir) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "raw path cache directory is not a real directory",
            ));
        }
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.uid() != unsafe { libc::getuid() } {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "raw path cache directory is not owned by the current user",
                    ));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&cache_dir)?;
        }
        Err(error) => return Err(error),
    }
    set_private_directory(&cache_dir)?;
    Ok(cache_dir)
}

fn open_secure_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        options.open(path)
    }
}

fn acquire_cache_lock(path: &Path) -> io::Result<File> {
    for _ in 0..3 {
        match open_secure_file(path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                file.sync_all()?;
                return Ok(file);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let observed = std::fs::read_to_string(path).unwrap_or_default();
                let pid = observed.trim().parse::<u32>().ok();
                #[cfg(target_os = "linux")]
                let owner_alive =
                    pid.is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
                #[cfg(not(target_os = "linux"))]
                let owner_alive = false;
                if owner_alive {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "raw path cache is already being written",
                    ));
                }
                if std::fs::read_to_string(path).unwrap_or_default() != observed {
                    continue;
                }
                let _ = std::fs::remove_file(path);
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "raw path cache lock is busy",
    ))
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = path;
    Ok(())
}

fn fish_pid_suffix() -> u32 {
    // Keep compatibility with callers that export the conventional uppercase
    // form while fish's native variable remains the preferred spelling.
    if let Some(value) = std::env::var_os("FISH_PID")
        && let Some(text) = value.to_str()
        && let Ok(pid) = text.parse::<u32>()
    {
        return pid;
    }
    if let Some(value) = std::env::var_os("fish_pid")
        && let Some(text) = value.to_str()
        && let Ok(pid) = text.parse::<u32>()
    {
        return pid;
    }

    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat")
        && let Some((_, after_comm)) = stat.rsplit_once(") ")
    {
        let mut fields = after_comm.split_whitespace();
        let _state = fields.next();
        if let Some(parent_pid) = fields.next().and_then(|value| value.parse().ok()) {
            return parent_pid;
        }
    }
    std::process::id()
}
