//! Read only filesystem access.
//!
//! Helm never mounts anything: the filesystems are parsed directly from the
//! raw partition, which keeps the helper free of mount side effects and lets
//! it look at the partitions of an operating system it is not running.

pub mod btrfs;
pub mod ext4;
pub mod fat;
pub mod memory;
pub mod ntfs;

use std::io;

use helm_protocol::FileSystemKind;

use crate::block::{BlockDevice, PartitionDevice};

/// An entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Errors raised while reading a filesystem.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("path not found: {0}")]
    NotFound(String),
    #[error("unsupported filesystem: {0}")]
    Unsupported(String),
    #[error("the filesystem is damaged: {0}")]
    Corrupt(String),
    #[error("{0}")]
    Other(String),
}

/// Read only access to a filesystem.
pub trait VolumeFs {
    /// List the content of a directory. The path uses `/` separators and is
    /// relative to the root of the filesystem.
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, FsError>;

    /// Read a file, stopping after `max` bytes.
    fn read_file(&mut self, path: &str, max: usize) -> Result<Vec<u8>, FsError>;

    /// Size of a file, when it exists.
    fn file_size(&mut self, path: &str) -> Option<u64>;

    /// True when the path exists.
    fn exists(&mut self, path: &str) -> bool {
        self.file_size(path).is_some()
    }

    /// Find the first entry of a directory whose name starts with `prefix`.
    fn find_prefix(&mut self, dir: &str, prefix: &str) -> Result<Vec<FsEntry>, FsError> {
        let entries = self.list_dir(dir)?;
        Ok(entries
            .into_iter()
            .filter(|entry| entry.name.starts_with(prefix))
            .collect())
    }
}

/// Split a path into its components.
pub fn split_path(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

/// Identify the filesystem of a partition by looking at its superblock.
///
/// The signature check is intentionally conservative: a partition is only
/// reported as a specific filesystem when the magic number matches.
pub fn detect_filesystem(device: &dyn BlockDevice) -> FileSystemKind {
    let head = read_up_to(device, 0, 4096);
    let head = &head[..];

    // NTFS: "NTFS    " at offset 3.
    if head.len() > 11 && &head[3..11] == b"NTFS    " {
        return FileSystemKind::Ntfs;
    }
    // exFAT.
    if head.len() > 11 && &head[3..11] == b"EXFAT   " {
        return FileSystemKind::Unknown;
    }
    // FAT: the filesystem type string sits at a different offset depending on
    // the size of the BIOS parameter block.
    if head.len() > 90 {
        let fat32 = &head[82..87];
        let fat16 = &head[54..62];
        let fat12 = &head[54..62];
        if fat32 == b"FAT32" {
            return FileSystemKind::Fat32;
        }
        if fat16.starts_with(b"FAT16") {
            return FileSystemKind::Fat16;
        }
        if fat12.starts_with(b"FAT12") {
            return FileSystemKind::Fat12;
        }
    }
    // ext2/3/4: the superblock starts at 1024, the magic is at 1080.
    if head.len() > 1082 && u16::from_le_bytes([head[1080], head[1081]]) == 0xEF53 {
        let superblock = &head[1024..];
        let feature_incompat = u32::from_le_bytes([
            superblock[0x60],
            superblock[0x61],
            superblock[0x62],
            superblock[0x63],
        ]);
        let has_extents = feature_incompat & 0x40 != 0; // EXTENTS
        let is_64bit = feature_incompat & 0x80 != 0; // 64BIT
        let is_journal = feature_incompat & 0x04 != 0; // JOURNAL
        let has_meta_bg = feature_incompat & 0x10 != 0; // META_BG
        if has_extents || is_64bit || has_meta_bg {
            return FileSystemKind::Ext4;
        }
        if is_journal {
            return FileSystemKind::Ext3;
        }
        return FileSystemKind::Ext2;
    }
    // XFS: the magic is at the very beginning of the volume.
    if head.len() > 4 && &head[0..4] == b"XFSB" {
        return FileSystemKind::Xfs;
    }
    // btrfs: the superblock lives at 64 KiB, the magic at 64 bytes inside it.
    let area = read_up_to(device, 64 * 1024, 64 + 8);
    if area.len() >= 64 + 8 && &area[64..64 + 8] == b"_BHRfS_M" {
        return FileSystemKind::Btrfs;
    }
    FileSystemKind::Unknown
}

/// Read at most `count` bytes, tolerating a short device.
fn read_up_to(device: &dyn BlockDevice, offset: u64, count: usize) -> Vec<u8> {
    let mut buf = vec![0u8; count];
    let mut read = 0usize;
    while read < count {
        match device.read_at(offset + read as u64, &mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    buf.truncate(read);
    buf
}

/// Open a filesystem on a partition.
pub fn open_filesystem(
    device: PartitionDevice,
    kind: FileSystemKind,
) -> Result<Box<dyn VolumeFs>, FsError> {
    match kind {
        FileSystemKind::Fat12 | FileSystemKind::Fat16 | FileSystemKind::Fat32 => {
            Ok(Box::new(fat::FatVolume::open(device)?))
        }
        FileSystemKind::Ext2 | FileSystemKind::Ext3 | FileSystemKind::Ext4 => {
            Ok(Box::new(ext4::Ext4Volume::open(device)?))
        }
        FileSystemKind::Ntfs => Ok(Box::new(ntfs::NtfsVolume::open(device)?)),
        FileSystemKind::Btrfs => Ok(Box::new(btrfs::BtrfsVolume::open(device)?)),
        other => Err(FsError::Unsupported(format!(
            "no reader for {other:?} filesystems"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::SliceDevice;

    #[test]
    fn path_splitting() {
        assert_eq!(split_path("/etc/os-release"), vec!["etc", "os-release"]);
        assert_eq!(split_path("/"), Vec::<&str>::new());
        assert_eq!(split_path(""), Vec::<&str>::new());
    }

    #[test]
    fn detects_ntfs_signature() {
        let mut image = vec![0u8; 512];
        image[3..11].copy_from_slice(b"NTFS    ");
        let device = SliceDevice::new(image);
        assert_eq!(detect_filesystem(&device), FileSystemKind::Ntfs);
    }

    #[test]
    fn detects_fat32_signature() {
        let mut image = vec![0u8; 512];
        image[82..87].copy_from_slice(b"FAT32");
        let device = SliceDevice::new(image);
        assert_eq!(detect_filesystem(&device), FileSystemKind::Fat32);
    }

    #[test]
    fn detects_ext4_signature() {
        let mut image = vec![0u8; 4096];
        image[1080..1082].copy_from_slice(&0xEF53u16.to_le_bytes());
        // EXTENTS feature flag (incompatible features at superblock offset 0x60).
        image[1024 + 0x60..1024 + 0x64].copy_from_slice(&0x40u32.to_le_bytes());
        let device = SliceDevice::new(image);
        assert_eq!(detect_filesystem(&device), FileSystemKind::Ext4);
    }

    #[test]
    fn detects_ext3_signature() {
        let mut image = vec![0u8; 4096];
        image[1080..1082].copy_from_slice(&0xEF53u16.to_le_bytes());
        // JOURNAL feature flag.
        image[1024 + 0x60..1024 + 0x64].copy_from_slice(&0x04u32.to_le_bytes());
        let device = SliceDevice::new(image);
        assert_eq!(detect_filesystem(&device), FileSystemKind::Ext3);
    }

    #[test]
    fn detects_xfs_signature() {
        let mut image = vec![0u8; 512];
        image[0..4].copy_from_slice(b"XFSB");
        let device = SliceDevice::new(image);
        assert_eq!(detect_filesystem(&device), FileSystemKind::Xfs);
    }

    #[test]
    fn unknown_for_an_empty_device() {
        let device = SliceDevice::new(vec![0u8; 8192]);
        assert_eq!(detect_filesystem(&device), FileSystemKind::Unknown);
    }
}
