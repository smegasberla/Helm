//! Read only access to FAT12/16/32 volumes (the EFI System Partition).

use std::io::Seek;
use std::sync::Arc;

use fatfs::{FileSystem, FsOptions};

use crate::block::{BlockDevice, DeviceCursor, PartitionDevice};
use crate::fs::{FsEntry, FsError, VolumeFs};

/// A FAT volume.
pub struct FatVolume {
    fs: FileSystem<DeviceCursor>,
}

impl FatVolume {
    pub fn open(device: PartitionDevice) -> Result<Self, FsError> {
        let cursor = DeviceCursor::new(Arc::new(device) as Arc<dyn BlockDevice>);
        let fs = FileSystem::new(cursor, FsOptions::new())
            .map_err(|err| FsError::Corrupt(format!("FAT volume: {err}")))?;
        Ok(Self { fs })
    }

    fn root(&self) -> fatfs::Dir<'_, DeviceCursor> {
        self.fs.root_dir()
    }
}

impl VolumeFs for FatVolume {
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, FsError> {
        let root = self.root();
        let dir = if path.is_empty() || path == "/" {
            root
        } else {
            root.open_dir(path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => FsError::NotFound(path.to_string()),
                _ => FsError::Other(format!("open {path}: {err}")),
            })?
        };

        let mut entries = Vec::new();
        for entry in dir.iter() {
            let entry = entry.map_err(|err| FsError::Other(format!("read dir {path}: {err}")))?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            entries.push(FsEntry {
                name,
                is_dir: entry.is_dir(),
                size: if entry.is_dir() { 0 } else { entry.len() },
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn read_file(&mut self, path: &str, max: usize) -> Result<Vec<u8>, FsError> {
        let root = self.root();
        let mut file = root.open_file(path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => FsError::NotFound(path.to_string()),
            _ => FsError::Other(format!("open {path}: {err}")),
        })?;
        let size = file_size(&mut file);
        // `file_size` seeks to the end of the file; go back to the start.
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|err| FsError::Other(format!("rewind {path}: {err}")))?;
        let len = size.min(max as u64) as usize;
        let mut buf = vec![0u8; len];
        let mut read = 0usize;
        while read < len {
            let n = std::io::Read::read(&mut file, &mut buf[read..])
                .map_err(|err| FsError::Other(format!("read {path}: {err}")))?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(buf)
    }

    fn file_size(&mut self, path: &str) -> Option<u64> {
        let root = self.root();
        let file = root.open_file(path).ok()?;
        let mut file = file;
        Some(file_size(&mut file))
    }
}

/// Determine the size of an open FAT file.
fn file_size(file: &mut fatfs::File<'_, DeviceCursor>) -> u64 {
    use std::io::Seek;
    file.seek(std::io::SeekFrom::End(0)).unwrap_or(0)
}
