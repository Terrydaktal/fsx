//! Content identity checks used by metadata-first collision policies.

use super::copy_engine::{interrupted, open_source_noatime};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
use std::path::Path;

/// Compare regular-file bytes without retaining a content cache.
pub(crate) fn regular_file_contents_equal(source: &Path, destination: &Path) -> io::Result<bool> {
    let source_meta = fs::metadata(source)?;
    let destination_meta = fs::metadata(destination)?;
    if !source_meta.is_file() || !destination_meta.is_file() {
        return Ok(false);
    }
    if source_meta.len() != destination_meta.len() {
        return Ok(false);
    }

    let mut source_file = open_source_noatime(source)?;
    let mut destination_file = fs::File::open(destination)?;
    let mut source_hash = Sha256::new();
    let mut destination_hash = Sha256::new();
    let mut source_buf = vec![0u8; 1024 * 1024];
    let mut destination_buf = vec![0u8; 1024 * 1024];
    loop {
        if interrupted() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "content comparison cancelled",
            ));
        }
        let source_read = source_file.read(&mut source_buf)?;
        let destination_read = destination_file.read(&mut destination_buf)?;
        if source_read != destination_read {
            return Ok(false);
        }
        if source_read == 0 {
            break;
        }
        source_hash.update(&source_buf[..source_read]);
        destination_hash.update(&destination_buf[..destination_read]);
    }
    Ok(source_hash.finalize() == destination_hash.finalize())
}
