//! Content identity checks used by metadata-first collision policies.

use super::copy_engine::{interrupted, open_source_noatime};
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
    streams_equal(
        &mut source_file,
        &mut destination_file,
        source_meta.len().clamp(1, 1024 * 1024) as usize,
    )
}

fn fill_chunk(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        if interrupted() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "content comparison cancelled",
            ));
        }
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn streams_equal(
    source: &mut impl Read,
    destination: &mut impl Read,
    chunk: usize,
) -> io::Result<bool> {
    let mut source_buf = vec![0u8; chunk];
    let mut destination_buf = vec![0u8; chunk];
    loop {
        if interrupted() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "content comparison cancelled",
            ));
        }
        let source_read = fill_chunk(source, &mut source_buf)?;
        let destination_read = fill_chunk(destination, &mut destination_buf)?;
        if source_read != destination_read
            || source_buf[..source_read] != destination_buf[..destination_read]
        {
            return Ok(false);
        }
        if source_read == 0 {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ShortReader<'a> {
        bytes: &'a [u8],
        max: usize,
        interrupted_once: bool,
    }
    impl Read for ShortReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted_once {
                self.interrupted_once = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let length = buffer.len().min(self.max);
            self.bytes.read(&mut buffer[..length])
        }
    }

    #[test]
    fn different_short_read_boundaries_do_not_imply_different_content() {
        let bytes = b"same bytes, differently sized reads";
        let mut a = ShortReader {
            bytes,
            max: 2,
            interrupted_once: false,
        };
        let mut b = ShortReader {
            bytes,
            max: 7,
            interrupted_once: false,
        };
        assert!(streams_equal(&mut a, &mut b, 8).unwrap());
    }

    #[test]
    fn mismatch_stops_after_first_chunk_and_detects_length_changes() {
        let mut a = &b"aaaa........"[..];
        let mut b = &b"baaa........"[..];
        assert!(!streams_equal(&mut a, &mut b, 4).unwrap());
        assert_eq!(a.len(), 8);
        assert_eq!(b.len(), 8);
        assert!(!streams_equal(&mut &b"short"[..], &mut &b"shorter"[..], 4).unwrap());
        assert!(streams_equal(&mut &b""[..], &mut &b""[..], 4).unwrap());
    }

    #[test]
    fn read_errors_are_not_treated_as_identity() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        }
        assert_eq!(
            streams_equal(&mut Broken, &mut &b"x"[..], 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
