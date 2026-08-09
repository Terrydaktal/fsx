//! Low-level local filesystem operations used by transfer workers.
//!
//! The engine handles data movement and metadata preservation only; planning,
//! collision decisions, progress policy, and cleanup remain in their modules.

use crate::domain::{ManifestDirTimeEntry, MediaKind};
use crate::plan::{map_dir_dest_path, normalize_rel};
use crate::runtime::copy_chunk_bytes_for_file;
use filetime::{set_file_times, FileTime};
use jwalk::WalkDir;
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use tempfile::TempPath;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static INSTALL_INTERRUPT_HANDLER: Once = Once::new();

#[cfg(unix)]
extern "C" fn handle_interrupt(_: nix::libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

pub(crate) fn install_interrupt_handler() {
    INSTALL_INTERRUPT_HANDLER.call_once(|| {
        #[cfg(unix)]
        unsafe {
            let handler = handle_interrupt as nix::libc::sighandler_t;
            let _ = nix::libc::signal(nix::libc::SIGINT, handler);
            let _ = nix::libc::signal(nix::libc::SIGTERM, handler);
        }
    });
}

pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// Compare the logical bytes of two regular files after a transfer.
///
/// This deliberately hashes both paths after publication rather than trusting
/// the copy primitive: reflinks, copy_file_range, sparse writes, and resumed
/// destinations all receive the same verification.
pub(crate) fn verify_regular_file_pair(source: &Path, destination: &Path) -> io::Result<()> {
    let source_meta = fs::metadata(source)?;
    let destination_meta = fs::metadata(destination)?;
    if !source_meta.is_file() || !destination_meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "verification requires regular files",
        ));
    }
    if source_meta.len() != destination_meta.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "verification failed: size differs ({} != {})",
                source_meta.len(),
                destination_meta.len()
            ),
        ));
    }

    let mut source_file = open_source_noatime(source)?;
    let mut destination_file = File::open(destination)?;
    let mut source_hash = Sha256::new();
    let mut destination_hash = Sha256::new();
    // Keep verification buffers on the heap; Rust's test worker stacks are
    // intentionally small and two 1 MiB arrays would overflow them.
    let mut source_buf = vec![0u8; 1024 * 1024];
    let mut destination_buf = vec![0u8; 1024 * 1024];
    loop {
        if interrupted() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "verification cancelled",
            ));
        }
        let source_read = source_file.read(&mut source_buf)?;
        let destination_read = destination_file.read(&mut destination_buf)?;
        if source_read != destination_read {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "verification failed: files ended at different offsets",
            ));
        }
        if source_read == 0 {
            break;
        }
        source_hash.update(&source_buf[..source_read]);
        destination_hash.update(&destination_buf[..destination_read]);
    }
    if source_hash.finalize() != destination_hash.finalize() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "verification failed: SHA-256 digest differs",
        ));
    }
    Ok(())
}

fn cancellation() -> Option<&'static AtomicBool> {
    Some(&INTERRUPTED)
}

pub(crate) fn open_source_noatime(path: &Path) -> io::Result<File> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOATIME)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(err)
            if matches!(
                err.raw_os_error(),
                Some(code)
                    if matches!(
                        code,
                        nix::libc::EPERM
                            | nix::libc::EACCES
                            | nix::libc::EINVAL
                            | nix::libc::EOPNOTSUPP
                    )
            ) =>
        {
            File::open(path)
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn ensure_no_symlink_ancestors(path: &Path) -> io::Result<()> {
    let mut current = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        PathBuf::from(".")
    };
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "parent traversal is not allowed for transfer paths",
                ));
            }
            std::path::Component::Normal(name) => current.push(name),
            std::path::Component::Prefix(_) => continue,
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("symlink ancestor is not allowed: {}", current.display()),
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("path ancestor is not a directory: {}", current.display()),
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_directory_nofollow(path: &Path) -> io::Result<File> {
    let mut current = File::open(if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    })?;
    for component in path.components() {
        let name = match component {
            std::path::Component::Normal(name) => name,
            std::path::Component::RootDir | std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "parent traversal is not allowed for transfer paths",
                ));
            }
            std::path::Component::Prefix(_) => continue,
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory name"))?;
        let fd = unsafe {
            nix::libc::openat(
                current.as_raw_fd(),
                name.as_ptr(),
                nix::libc::O_RDONLY
                    | nix::libc::O_DIRECTORY
                    | nix::libc::O_NOFOLLOW
                    | nix::libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        current = unsafe { File::from_raw_fd(fd) };
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
fn open_destination_for_update(destination: &Path) -> io::Result<File> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let follow_final_symlink = fs::symlink_metadata(destination)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false);
    let parent_fd = open_directory_nofollow(parent)?;
    let name = CString::new(
        destination
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no name"))?
            .as_bytes(),
    )
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
    let mut flags =
        nix::libc::O_WRONLY | nix::libc::O_CREAT | nix::libc::O_TRUNC | nix::libc::O_CLOEXEC;
    if !follow_final_symlink {
        flags |= nix::libc::O_NOFOLLOW;
    }
    let fd = unsafe { nix::libc::openat(parent_fd.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn open_destination_for_update(destination: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(destination)
}

#[cfg(target_os = "linux")]
fn persist_temp_path(staged: TempPath, destination: &Path) -> io::Result<()> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let temp_name = staged
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "staged file has no name"))?;
    let destination_name = destination
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no name"))?;
    let temp_bytes = CString::new(temp_name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid staged name"))?;
    let destination_bytes = CString::new(destination_name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
    let directory = open_directory_nofollow(parent)?;
    let directory_fd = directory.as_raw_fd();
    let result = unsafe {
        nix::libc::renameat(
            directory_fd,
            temp_bytes.as_ptr(),
            directory_fd,
            destination_bytes.as_ptr(),
        )
    };
    let error = if result == 0 {
        None
    } else {
        Some(io::Error::last_os_error())
    };
    if let Some(error) = error {
        return Err(error);
    }
    std::mem::forget(staged);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn persist_temp_path(staged: TempPath, destination: &Path) -> io::Result<()> {
    staged.persist(destination).map_err(|error| error.error)
}

pub(crate) fn apply_file_metadata_fd(file: &File, meta: &fs::Metadata) -> io::Result<()> {
    let mode = meta.mode() & 0o7777;
    if unsafe { nix::libc::fchmod(file.as_raw_fd(), mode as nix::libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let times = [
        nix::libc::timespec {
            tv_sec: meta.atime(),
            tv_nsec: meta.atime_nsec(),
        },
        nix::libc::timespec {
            tv_sec: meta.mtime(),
            tv_nsec: meta.mtime_nsec(),
        },
    ];
    if unsafe { nix::libc::futimens(file.as_raw_fd(), times.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn try_reflink(src: &File, dst: &File) -> bool {
    const FICLONE: nix::libc::c_ulong = 0x4004_9409;
    unsafe { nix::libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) == 0 }
}

pub(crate) fn advise_sequential(file: &File) {
    unsafe {
        let _ = nix::libc::posix_fadvise(file.as_raw_fd(), 0, 0, nix::libc::POSIX_FADV_SEQUENTIAL);
    }
}

pub(crate) fn advise_drop_cache(file: &File) {
    unsafe {
        let _ = nix::libc::posix_fadvise(file.as_raw_fd(), 0, 0, nix::libc::POSIX_FADV_DONTNEED);
    }
}

pub(crate) fn pace_hdd_writeback(file: &File, total: u64, next_pace_at: &mut u64) {
    const STEP: u64 = 32 * 1024 * 1024;
    const WINDOW: u64 = 96 * 1024 * 1024;
    if total < *next_pace_at {
        return;
    }
    let start = total.saturating_sub(WINDOW);
    const WAIT_BEFORE: u32 = 1;
    const WRITE: u32 = 2;
    unsafe {
        let _ = nix::libc::sync_file_range(
            file.as_raw_fd(),
            start as nix::libc::off64_t,
            STEP as nix::libc::off64_t,
            WAIT_BEFORE | WRITE,
        );
    }
    *next_pace_at = total.saturating_add(STEP);
}

pub(crate) fn copy_sparse_extents<F>(
    src: &mut File,
    dst: &mut File,
    size: u64,
    buf: &mut [u8],
    media: MediaKind,
    cancelled: Option<&AtomicBool>,
    on_bytes: &mut F,
) -> io::Result<Option<u64>>
where
    F: FnMut(u64),
{
    if size == 0 {
        dst.set_len(0)?;
        return Ok(Some(0));
    }
    let first_data = unsafe { nix::libc::lseek(src.as_raw_fd(), 0, nix::libc::SEEK_DATA) };
    if first_data < 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(nix::libc::ENXIO) => {
                dst.set_len(size)?;
                Ok(Some(size))
            }
            Some(nix::libc::EINVAL | nix::libc::ENOTSUP) => Ok(None),
            _ => Err(io::Error::last_os_error()),
        };
    }

    dst.set_len(size)?;
    let mut data = first_data as u64;
    let mut logical_done = data;
    if data > 0 {
        on_bytes(data);
    }
    let mut next_pace_at = 32 * 1024 * 1024;
    while data < size {
        let hole = unsafe {
            nix::libc::lseek(
                src.as_raw_fd(),
                data as nix::libc::off_t,
                nix::libc::SEEK_HOLE,
            )
        };
        if hole < 0 {
            return Err(io::Error::last_os_error());
        }
        let hole = (hole as u64).min(size);
        src.seek(SeekFrom::Start(data))?;
        dst.seek(SeekFrom::Start(data))?;
        let mut remaining = hole.saturating_sub(data);
        while remaining > 0 {
            if cancelled
                .map(|cancelled| cancelled.load(Ordering::Relaxed))
                .unwrap_or(false)
            {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "copy cancelled"));
            }
            let want = remaining.min(buf.len() as u64) as usize;
            let n = src.read(&mut buf[..want])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "sparse extent ended early",
                ));
            }
            dst.write_all(&buf[..n])?;
            let n = n as u64;
            remaining -= n;
            logical_done = logical_done.saturating_add(n);
            on_bytes(n);
            if media == MediaKind::Hdd {
                pace_hdd_writeback(dst, logical_done, &mut next_pace_at);
            }
        }
        if hole >= size {
            break;
        }
        let next_data = unsafe {
            nix::libc::lseek(
                src.as_raw_fd(),
                hole as nix::libc::off_t,
                nix::libc::SEEK_DATA,
            )
        };
        if next_data < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(nix::libc::ENXIO) {
                let tail = size.saturating_sub(hole);
                on_bytes(tail);
                logical_done = logical_done.saturating_add(tail);
                break;
            }
            return Err(io::Error::last_os_error());
        }
        let next_data = next_data as u64;
        let hole_bytes = next_data.saturating_sub(hole);
        on_bytes(hole_bytes);
        logical_done = logical_done.saturating_add(hole_bytes);
        data = next_data;
    }
    Ok(Some(logical_done.min(size)))
}

pub(crate) fn copy_file_range_all<F>(
    src: &File,
    dst: &File,
    size: u64,
    cancelled: Option<&AtomicBool>,
    on_bytes: &mut F,
) -> io::Result<Option<u64>>
where
    F: FnMut(u64),
{
    let mut total = 0u64;
    while total < size {
        if cancelled
            .map(|cancelled| cancelled.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "copy cancelled"));
        }
        let count = (size - total).min(16 * 1024 * 1024) as usize;
        let copied = unsafe {
            nix::libc::copy_file_range(
                src.as_raw_fd(),
                std::ptr::null_mut(),
                dst.as_raw_fd(),
                std::ptr::null_mut(),
                count,
                0,
            )
        };
        if copied < 0 {
            let err = io::Error::last_os_error();
            if total == 0
                && matches!(
                    err.raw_os_error(),
                    Some(
                        nix::libc::EXDEV
                            | nix::libc::EINVAL
                            | nix::libc::ENOSYS
                            | nix::libc::EOPNOTSUPP
                    )
                )
            {
                return Ok(None);
            }
            return Err(err);
        }
        if copied == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "copy_file_range ended before the planned file size",
            ));
        }
        let copied = copied as u64;
        total += copied;
        on_bytes(copied);
    }
    Ok(Some(total))
}

pub(crate) fn copy_file_preserve_with_progress_buffer<F>(
    src: &Path,
    dst: &Path,
    media: MediaKind,
    reusable_buf: &mut Vec<u8>,
    on_bytes: F,
) -> io::Result<u64>
where
    F: FnMut(u64),
{
    copy_file_preserve_with_progress_buffer_inner(
        src,
        dst,
        media,
        reusable_buf,
        false,
        cancellation(),
        on_bytes,
    )
}

pub(crate) fn copy_file_preserve_atomic_with_progress_buf<F>(
    src: &Path,
    dst: &Path,
    media: MediaKind,
    buf_bytes: usize,
    on_bytes: F,
) -> io::Result<u64>
where
    F: FnMut(u64),
{
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    ensure_no_symlink_ancestors(parent)?;
    fs::create_dir_all(parent)?;
    ensure_no_symlink_ancestors(parent)?;
    let staged = tempfile::Builder::new()
        .prefix(".copy-rs-partial-")
        .tempfile_in(parent)?
        .into_temp_path();
    let mut buf = vec![0u8; buf_bytes.max(64 * 1024)];
    let copied = copy_file_preserve_with_progress_buffer_inner(
        src,
        staged.as_ref(),
        media,
        &mut buf,
        true,
        cancellation(),
        on_bytes,
    )?;

    if fs::symlink_metadata(dst)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false)
    {
        fs::remove_dir_all(dst)?;
    }
    persist_temp_path(staged, dst)?;
    Ok(copied)
}

pub(crate) fn copy_file_preserve_with_progress_buffer_inner<F>(
    src: &Path,
    dst: &Path,
    media: MediaKind,
    reusable_buf: &mut Vec<u8>,
    parent_ready: bool,
    cancelled: Option<&AtomicBool>,
    mut on_bytes: F,
) -> io::Result<u64>
where
    F: FnMut(u64),
{
    if cancelled
        .map(|cancelled| cancelled.load(Ordering::Relaxed))
        .unwrap_or(false)
    {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "copy cancelled"));
    }
    let meta = fs::symlink_metadata(src)?;
    if !parent_ready {
        if let Some(parent) = dst.parent() {
            ensure_no_symlink_ancestors(parent)?;
            fs::create_dir_all(parent)?;
            ensure_no_symlink_ancestors(parent)?;
        }
    }
    let mut in_file = open_source_noatime(src)?;
    let mut out_file = open_destination_for_update(dst)?;
    advise_sequential(&in_file);

    let desired = copy_chunk_bytes_for_file(media, meta.len()).max(64 * 1024);
    if reusable_buf.len() < desired {
        reusable_buf.resize(desired, 0);
    }
    let buf = reusable_buf;
    let sparse = meta.blocks().saturating_mul(512) < meta.len();

    let total = if meta.len() > 0 && try_reflink(&in_file, &out_file) {
        on_bytes(meta.len());
        meta.len()
    } else if sparse {
        match copy_sparse_extents(
            &mut in_file,
            &mut out_file,
            meta.len(),
            buf,
            media,
            cancelled,
            &mut on_bytes,
        )? {
            Some(total) => total,
            None => {
                in_file.seek(SeekFrom::Start(0))?;
                out_file.set_len(0)?;
                out_file.seek(SeekFrom::Start(0))?;
                copy_buffered(
                    &mut in_file,
                    &mut out_file,
                    buf,
                    media,
                    cancelled,
                    &mut on_bytes,
                )?
            }
        }
    } else {
        if meta.len() >= 64 * 1024 * 1024 {
            unsafe {
                let _ = nix::libc::posix_fallocate(
                    out_file.as_raw_fd(),
                    0,
                    meta.len() as nix::libc::off_t,
                );
            }
        }
        match copy_file_range_all(&in_file, &out_file, meta.len(), cancelled, &mut on_bytes)? {
            Some(total) => total,
            None => {
                in_file.seek(SeekFrom::Start(0))?;
                out_file.set_len(0)?;
                out_file.seek(SeekFrom::Start(0))?;
                copy_buffered(
                    &mut in_file,
                    &mut out_file,
                    buf,
                    media,
                    cancelled,
                    &mut on_bytes,
                )?
            }
        }
    };

    out_file.flush()?;
    apply_file_metadata_fd(&out_file, &meta)?;
    advise_drop_cache(&in_file);
    Ok(total)
}

pub(crate) fn copy_buffered<F>(
    src: &mut File,
    dst: &mut File,
    buf: &mut [u8],
    media: MediaKind,
    cancelled: Option<&AtomicBool>,
    on_bytes: &mut F,
) -> io::Result<u64>
where
    F: FnMut(u64),
{
    let mut total = 0u64;
    let mut next_pace_at = 32 * 1024 * 1024;
    loop {
        if cancelled
            .map(|cancelled| cancelled.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "copy cancelled"));
        }
        let n = src.read(buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        let n = n as u64;
        total += n;
        on_bytes(n);
        if media == MediaKind::Hdd {
            pace_hdd_writeback(dst, total, &mut next_pace_at);
        }
    }
    Ok(total)
}

pub(crate) fn copy_file_preserve_with_progress_buf<F>(
    src: &Path,
    dst: &Path,
    buf_bytes: usize,
    on_bytes: F,
) -> io::Result<u64>
where
    F: FnMut(u64),
{
    let mut buf = vec![0u8; buf_bytes.max(64 * 1024)];
    copy_file_preserve_with_progress_buffer(src, dst, MediaKind::Other, &mut buf, on_bytes)
}

pub(crate) fn copy_file_preserve_with_progress<F>(
    src: &Path,
    dst: &Path,
    on_bytes: F,
) -> io::Result<u64>
where
    F: FnMut(u64),
{
    copy_file_preserve_with_progress_buf(src, dst, 1024 * 1024, on_bytes)
}

pub(crate) fn copy_file_preserve(src: &Path, dst: &Path) -> io::Result<u64> {
    copy_file_preserve_with_progress(src, dst, |_| {})
}

pub(crate) fn remove_path_local_if_exists(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(md) => {
            if md.file_type().is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[allow(dead_code)]
pub(crate) fn unlinkat_file_if_exists(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name contains NUL"))?;
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_CLOEXEC)
        .open(parent)?;
    let rc = unsafe { nix::libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else if matches!(
        err.raw_os_error(),
        Some(nix::libc::EISDIR | nix::libc::EPERM)
    ) {
        remove_path_local_if_exists(path)
    } else {
        Err(err)
    }
}

pub(crate) fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    copy_symlink_atomic(src, dst)
}

pub(crate) fn copy_symlink_atomic(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    ensure_no_symlink_ancestors(parent)?;
    fs::create_dir_all(parent)?;
    let staged = tempfile::Builder::new()
        .prefix(".copy-rs-partial-")
        .tempfile_in(parent)?
        .into_temp_path();
    let staged_path: &Path = staged.as_ref();
    fs::remove_file(staged_path)?;
    symlink(target, staged_path)?;

    if fs::symlink_metadata(dst)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false)
    {
        fs::remove_dir_all(dst)?;
    }
    persist_temp_path(staged, dst)?;
    Ok(())
}

pub(crate) fn copy_hardlink_atomic(existing: &Path, dst: &Path) -> io::Result<()> {
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    ensure_no_symlink_ancestors(parent)?;
    fs::create_dir_all(parent)?;
    let staged = tempfile::Builder::new()
        .prefix(".copy-rs-partial-")
        .tempfile_in(parent)?
        .into_temp_path();
    let staged_path: &Path = staged.as_ref();
    fs::remove_file(staged_path)?;
    fs::hard_link(existing, staged_path)?;
    if fs::symlink_metadata(dst)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false)
    {
        fs::remove_dir_all(dst)?;
    }
    persist_temp_path(staged, dst)?;
    Ok(())
}

pub(crate) fn ensure_directory_target(path: &Path, replace_conflict: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_no_symlink_ancestors(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) if replace_conflict => {
            remove_path_local_if_exists(path)?;
            fs::create_dir_all(path)
        }
        Ok(_) => fs::create_dir_all(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path),
        Err(err) => Err(err),
    }
}

pub(crate) fn copy_path_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        copy_symlink_atomic(src, dst)?;
        return Ok(());
    }
    if meta.is_file() {
        let _ = copy_file_preserve(src, dst)?;
        return Ok(());
    }

    ensure_no_symlink_ancestors(dst.parent().unwrap_or_else(|| Path::new(".")))?;
    fs::create_dir_all(dst)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(meta.permissions().mode()))?;
    for ent in fs::read_dir(src)? {
        let ent = ent?;
        let child_src = ent.path();
        let child_dst = dst.join(ent.file_name());
        copy_path_recursive(&child_src, &child_dst)?;
    }
    let atime = FileTime::from_last_access_time(&meta);
    let mtime = FileTime::from_last_modification_time(&meta);
    let _ = set_file_times(dst, atime, mtime);
    Ok(())
}

pub(crate) fn preserve_directory_times_tree(
    src_root: &Path,
    dst_base: &Path,
    include_root: bool,
    src_base: &str,
    dir_times: Option<&[ManifestDirTimeEntry]>,
) -> io::Result<()> {
    if let Some(entries) = dir_times {
        // Manifest entries are stored in postorder so child timestamps are set first.
        for entry in entries {
            let dst_dir = map_dir_dest_path(include_root, src_base, &entry.rel, dst_base);
            set_directory_times_checked(&dst_dir, entry.atime, entry.mtime)?;
        }
        return Ok(());
    }

    let mut dirs: Vec<(PathBuf, FileTime, FileTime)> = Vec::new();
    for entry in WalkDir::new(src_root)
        .sort(false)
        .skip_hidden(false)
        .into_iter()
    {
        let entry = entry.map_err(|err| io::Error::other(err.to_string()))?;
        if !entry.file_type().is_dir() {
            continue;
        }
        // Read metadata while the walker still owns the entry. Looking up all
        // directories after traversal can observe atime after read_dir has
        // already updated it.
        let metadata = entry
            .metadata()
            .or_else(|_| fs::symlink_metadata(entry.path()))?;
        dirs.push((
            entry.path().to_path_buf(),
            FileTime::from_last_access_time(&metadata),
            FileTime::from_last_modification_time(&metadata),
        ));
    }
    dirs.sort_by_key(|(path, _, _)| std::cmp::Reverse(path.components().count()));

    for (src_dir, atime, mtime) in dirs {
        let rel = normalize_rel(src_dir.strip_prefix(src_root).unwrap_or(Path::new("")));
        let dst_dir = map_dir_dest_path(include_root, src_base, &rel, dst_base);
        set_directory_times_checked(&dst_dir, atime, mtime)?;
    }
    Ok(())
}

fn set_directory_times_checked(path: &Path, atime: FileTime, mtime: FileTime) -> io::Result<()> {
    ensure_no_symlink_ancestors(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!(
                "directory timestamp target is not a directory: {}",
                path.display()
            ),
        ));
    }
    set_file_times(path, atime, mtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_detects_post_copy_corruption() {
        let root = tempfile::tempdir().expect("temporary directory");
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::write(&source, b"verified bytes").expect("write source");
        copy_file_preserve_atomic_with_progress_buf(
            &source,
            &destination,
            MediaKind::Other,
            4096,
            |_| {},
        )
        .expect("copy source");
        verify_regular_file_pair(&source, &destination).expect("matching files verify");
        fs::write(&destination, b"corrupt").expect("corrupt destination");
        assert!(verify_regular_file_pair(&source, &destination).is_err());
    }
}
