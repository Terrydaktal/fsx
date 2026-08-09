use std::fs::{self, FileType, Metadata};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    pub fn from_file_type(file_type: FileType) -> Self {
        if file_type.is_file() {
            Self::File
        } else if file_type.is_dir() {
            Self::Directory
        } else if file_type.is_symlink() {
            Self::Symlink
        } else {
            Self::Other
        }
    }

    pub fn is_file(self) -> bool {
        self == Self::File
    }

    pub fn is_dir(self) -> bool {
        self == Self::Directory
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct HardlinkKey {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug)]
pub struct MetadataSnapshot {
    pub kind: EntryKind,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub links: Option<u64>,
    pub mode: Option<u32>,
    pub modified: Option<SystemTime>,
}

impl MetadataSnapshot {
    pub fn hardlink_key(&self) -> Option<HardlinkKey> {
        Some(HardlinkKey {
            device: self.device?,
            inode: self.inode?,
        })
    }

    pub fn is_hardlink_candidate(&self) -> bool {
        self.links.is_some_and(|links| links > 1) && self.hardlink_key().is_some()
    }
}

pub fn metadata_snapshot(metadata: &Metadata) -> MetadataSnapshot {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[cfg(unix)]
    let (device, inode, links, blocks, mode) = (
        Some(metadata.dev()),
        Some(metadata.ino()),
        Some(metadata.nlink()),
        metadata.blocks(),
        Some(metadata.permissions().mode()),
    );

    #[cfg(not(unix))]
    let (device, inode, links, blocks, mode) = (None, None, None, 0, None);

    MetadataSnapshot {
        kind: EntryKind::from_file_type(metadata.file_type()),
        logical_size: metadata.len(),
        allocated_size: if cfg!(unix) {
            blocks.saturating_mul(512)
        } else {
            metadata.len()
        },
        device,
        inode,
        links,
        mode,
        modified: metadata.modified().ok(),
    }
}

pub fn metadata_snapshot_for(path: &std::path::Path) -> std::io::Result<MetadataSnapshot> {
    fs::symlink_metadata(path).map(|metadata| metadata_snapshot(&metadata))
}

/// Return the logical byte size without constructing a metadata snapshot.
#[inline]
pub fn logical_size(metadata: &Metadata) -> u64 {
    metadata.len()
}

/// Return the allocated byte size using the same block semantics as `du`.
#[inline]
pub fn allocated_size(metadata: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        metadata.len()
    }
}

#[inline]
pub fn is_hardlink_candidate(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() > 1
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}
