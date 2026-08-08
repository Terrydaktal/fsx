use super::*;
use std::os::unix::fs::OpenOptionsExt;
pub(crate) fn snapshot_args_key_parts() -> Vec<String> {
    env::args()
        .skip(1)
        .filter(|arg| arg != "--snapshot-cache" && arg != "--snapshot-refresh")
        .collect()
}

pub(crate) fn snapshot_cache_path() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    for arg in snapshot_args_key_parts() {
        arg.hash(&mut hasher);
    }
    Some(snapshot_cache_dir()?.join(format!("{:016x}.paths", hasher.finish())))
}

pub(crate) fn snapshot_lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

pub(crate) fn snapshot_is_stale(path: &Path) -> bool {
    path_age_at_least(path, SNAPSHOT_REFRESH_MIN_AGE)
}

pub(crate) fn stream_snapshot_cache(path: &Path) -> io::Result<()> {
    let mut input = File::open(path)?;
    let stdout = io::stdout();
    let mut output = BufWriter::with_capacity(128 * 1024, stdout.lock());
    io::copy(&mut input, &mut output)?;
    output.flush()
}

pub(crate) fn write_snapshot_cache(path: &Path, lines: &[String]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", unique_temp_tag()));
    {
        let file = File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        let mut writer = BufWriter::with_capacity(128 * 1024, file);
        for line in lines {
            writeln!(writer, "{}", line)?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    fs::rename(tmp, path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn snapshot_owner_is_alive(contents: &str) -> bool {
    let mut fields = contents.split_whitespace();
    let Some(pid) = fields.next().and_then(|value| value.parse::<i64>().ok()) else {
        return false;
    };
    let Some(starttime) = fields.next().and_then(|value| value.parse::<i64>().ok()) else {
        return false;
    };
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, tail)| tail.split_whitespace().nth(19))
        .and_then(|value| value.parse::<i64>().ok())
        == Some(starttime)
}

fn acquire_snapshot_lock(path: &Path) -> io::Result<File> {
    for _ in 0..2 {
        match File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut file) => {
                let pid = std::process::id();
                let starttime = fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|stat| {
                        stat.rsplit_once(") ")?
                            .1
                            .split_whitespace()
                            .nth(19)?
                            .parse::<i64>()
                            .ok()
                    })
                    .unwrap_or_default();
                writeln!(file, "{pid} {starttime}")?;
                file.sync_all()?;
                return Ok(file);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let stale = fs::read_to_string(path)
                    .map(|contents| !snapshot_owner_is_alive(&contents))
                    .unwrap_or(true);
                if stale {
                    let _ = fs::remove_file(path);
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "snapshot refresh lock is busy",
    ))
}

pub(crate) fn spawn_snapshot_refresh() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(cache_path) = snapshot_cache_path() else {
        return;
    };
    if cache_path.is_file() && !snapshot_is_stale(&cache_path) {
        return;
    }
    let lock_path = snapshot_lock_path(&cache_path);
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if acquire_snapshot_lock(&lock_path).is_err() {
        return;
    }
    let mut args = snapshot_args_key_parts();
    args.push("--snapshot-refresh".to_string());
    if Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_err()
    {
        let _ = fs::remove_file(lock_path);
    }
}
