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
    sequence: u64,
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
        let operation_id = format!("{}-{timestamp}", std::process::id());
        let path = directory.join(format!("operation-{operation_id}.journal"));
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(file, "version=2")?;
        writeln!(file, "operation_id={operation_id}")?;
        writeln!(
            file,
            "build_git_sha={}",
            fsx::build_info::current(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")).git_sha
        )?;
        writeln!(file, "started_unix_ms={}", timestamp / 1_000_000)?;
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
            sequence: 0,
        })
    }

    pub(crate) fn mark(&mut self, state: &str) -> io::Result<()> {
        self.sequence = self.sequence.saturating_add(1);
        writeln!(self.file, "state={state}")?;
        writeln!(self.file, "transition_sequence={}", self.sequence)?;
        writeln!(
            self.file,
            "transition_unix_ms={}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        )?;
        self.file.sync_all()?;
        crash_test_boundary(state);
        Ok(())
    }

    pub(crate) fn record_result(&mut self, result: i32) -> io::Result<()> {
        writeln!(self.file, "result={result}")?;
        self.file.sync_all()
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

#[cfg(feature = "diagnostic-hooks")]
fn crash_test_boundary(boundary: &str) {
    if std::env::var("COPY_RS_TEST_CRASH_AT").as_deref() == Ok(boundary) {
        // Test-only failpoint: `_exit` deliberately skips destructors so the
        // integration suite observes the same journal state as a hard crash.
        unsafe { nix::libc::_exit(86) }
    }
}

#[cfg(not(feature = "diagnostic-hooks"))]
#[inline(always)]
fn crash_test_boundary(_boundary: &str) {}

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
        if matches!(
            state.as_deref(),
            Some("transferring" | "published" | "failed" | "aborted:source-read-preflight-failed")
        ) {
            eprintln!(
                "copy: incomplete operation journal retained at {}",
                entry.path().display()
            );
        }
    }

    let mut journals = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "journal"))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    journals.sort_by_key(|(modified, _)| *modified);
    const MAX_RETAINED_JOURNALS: usize = 128;
    let excess = journals.len().saturating_sub(MAX_RETAINED_JOURNALS);
    for (_, path) in journals.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}
