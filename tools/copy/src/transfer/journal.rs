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
            .write(true)
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
        Ok(Self {
            path,
            file,
            completed: false,
        })
    }

    pub(crate) fn mark(&mut self, state: &str) -> io::Result<()> {
        writeln!(self.file, "state={state}")?;
        self.file.sync_all()
    }

    pub(crate) fn complete(mut self) -> io::Result<()> {
        self.mark("complete")?;
        self.completed = true;
        fs::remove_file(&self.path)
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
            .is_some_and(|extension| extension == "journal")
        {
            eprintln!(
                "copy: incomplete operation journal retained at {}",
                entry.path().display()
            );
        }
    }
}
