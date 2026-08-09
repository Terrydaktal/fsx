use crate::metadata::MetadataSnapshot;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct EntrySnapshot {
    pub path: PathBuf,
    pub name: OsString,
    pub depth: usize,
    pub metadata: MetadataSnapshot,
}
