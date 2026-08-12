//! Durable local-operation journal for crash and interruption recovery.

use crate::domain::TransferMode;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct TransferJournal {
    path: PathBuf,
    file: File,
    completed: bool,
}

impl TransferJournal {
    pub(crate) fn begin(source: &Path, destination: &Path, mode: TransferMode) -> io::Result<Self> {
        let sources = [source.to_path_buf()];
        Self::begin_many(&sources, destination, mode)
    }

    pub(crate) fn begin_many(
        sources: &[PathBuf],
        destination: &Path,
        mode: TransferMode,
    ) -> io::Result<Self> {
        let directory = journal_directory();
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        report_stale_journals(&directory);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = directory.join(format!(
            "operation-{}-{timestamp}.journal",
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(file, "version=1")?;
        writeln!(file, "mode={}", mode.word())?;
        for source in sources {
            writeln!(file, "source={}", fsx::encode_lossless_path(source))?;
        }
        writeln!(
            file,
            "destination={}",
            fsx::encode_lossless_path(destination)
        )?;
        writeln!(file, "state=planned")?;
        file.sync_all()?;
        crash_test_boundary("planned");
        Ok(Self {
            path,
            file,
            completed: false,
        })
    }

    pub(crate) fn mark(&mut self, state: &str) -> io::Result<()> {
        writeln!(self.file, "state={state}")?;
        self.file.sync_all()?;
        crash_test_boundary(state);
        Ok(())
    }

    pub(crate) fn complete(mut self) -> io::Result<()> {
        self.mark("complete")?;
        self.completed = true;
        fs::remove_file(&self.path)
    }

    pub(crate) fn abandon(mut self, reason: &str) -> io::Result<()> {
        self.mark(&format!("aborted:{reason}"))?;
        self.completed = true;
        fs::remove_file(&self.path)
    }
}

fn crash_test_boundary(boundary: &str) {
    if std::env::var("COPY_RS_TEST_CRASH_AT").as_deref() == Ok(boundary) {
        // Test-only failpoint: `_exit` deliberately skips destructors so the
        // integration suite observes the same journal state as a hard crash.
        unsafe { nix::libc::_exit(86) }
    }
}

impl Drop for TransferJournal {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.file.sync_all();
        }
    }
}

fn journal_directory() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| std::env::temp_dir().join("copy-rs-state"))
        .join("copy-rs")
}

fn report_stale_journals(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "journal")
        {
            continue;
        }
        let state = fs::read_to_string(entry.path()).ok().and_then(|contents| {
            contents
                .lines()
                .rev()
                .find_map(|line| line.strip_prefix("state="))
                .map(str::to_owned)
        });
        if matches!(state.as_deref(), Some("transferring" | "published")) {
            eprintln!(
                "copy: incomplete operation journal retained at {}",
                entry.path().display()
            );
        }
    }
}
