//! Read only access to NTFS volumes (the Windows installations).

use std::sync::Arc;

use ntfs::Ntfs;

use crate::block::{BlockDevice, DeviceCursor, PartitionDevice};
use crate::fs::{split_path, FsEntry, FsError, VolumeFs};

/// An NTFS volume.
pub struct NtfsVolume {
    ntfs: Ntfs,
    cursor: DeviceCursor,
}

impl NtfsVolume {
    pub fn open(device: PartitionDevice) -> Result<Self, FsError> {
        let mut cursor = DeviceCursor::new(Arc::new(device) as Arc<dyn BlockDevice>);
        let ntfs = Ntfs::new(&mut cursor)
            .map_err(|err| FsError::Corrupt(format!("NTFS boot sector: {err}")))?;
        Ok(Self { ntfs, cursor })
    }

    /// Resolve a path to the file record number of its last component.
    fn resolve(&mut self, path: &str) -> Result<u64, FsError> {
        let parts = split_path(path);
        let mut current = self
            .ntfs
            .root_directory(&mut self.cursor)
            .map_err(|err| FsError::Corrupt(format!("NTFS root: {err}")))?;

        for part in parts {
            let index = current
                .directory_index(&mut self.cursor)
                .map_err(|err| FsError::Corrupt(format!("NTFS index: {err}")))?;
            let mut entries = index.entries();
            let mut found = None;
            while let Some(entry) = entries.next(&mut self.cursor) {
                let entry = entry.map_err(|err| FsError::Corrupt(format!("NTFS entry: {err}")))?;
                let Some(key) = entry.key() else { continue };
                let key = key.map_err(|err| FsError::Corrupt(format!("NTFS key: {err}")))?;
                if key.name().to_string_lossy().eq_ignore_ascii_case(part) {
                    found = Some(entry.file_reference().file_record_number());
                    break;
                }
            }
            let record = found.ok_or_else(|| FsError::NotFound(path.to_string()))?;
            current = self
                .ntfs
                .file(&mut self.cursor, record)
                .map_err(|err| FsError::Corrupt(format!("NTFS file {part}: {err}")))?;
        }

        Ok(current.file_record_number())
    }
}

impl VolumeFs for NtfsVolume {
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, FsError> {
        let record = self.resolve(path)?;
        let file = self
            .ntfs
            .file(&mut self.cursor, record)
            .map_err(|err| FsError::Corrupt(format!("NTFS file: {err}")))?;
        if !file.is_directory() {
            return Err(FsError::Other(format!("{path} is not a directory")));
        }

        let index = file
            .directory_index(&mut self.cursor)
            .map_err(|err| FsError::Corrupt(format!("NTFS index: {err}")))?;
        let mut entries = index.entries();
        let mut out = Vec::new();
        while let Some(entry) = entries.next(&mut self.cursor) {
            let entry = entry.map_err(|err| FsError::Corrupt(format!("NTFS entry: {err}")))?;
            let Some(key) = entry.key() else { continue };
            let key = key.map_err(|err| FsError::Corrupt(format!("NTFS key: {err}")))?;
            let name = key.name().to_string_lossy();
            if name.is_empty() {
                continue;
            }
            let child = self
                .ntfs
                .file(
                    &mut self.cursor,
                    entry.file_reference().file_record_number(),
                )
                .map_err(|err| FsError::Corrupt(format!("NTFS file {name}: {err}")))?;
            out.push(FsEntry {
                name,
                is_dir: child.is_directory(),
                size: child.data_size() as u64,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn read_file(&mut self, path: &str, max: usize) -> Result<Vec<u8>, FsError> {
        let record = self.resolve(path)?;
        let file = self
            .ntfs
            .file(&mut self.cursor, record)
            .map_err(|err| FsError::Corrupt(format!("NTFS file: {err}")))?;
        let attribute = file
            .data(&mut self.cursor, "")
            .ok_or_else(|| FsError::NotFound(path.to_string()))?
            .map_err(|err| FsError::Corrupt(format!("NTFS attribute: {err}")))?;
        let attribute = attribute
            .to_attribute()
            .map_err(|err| FsError::Corrupt(format!("NTFS attribute: {err}")))?;
        let value = attribute
            .value(&mut self.cursor)
            .map_err(|err| FsError::Corrupt(format!("NTFS value: {err}")))?;
        let mut reader = value.attach(&mut self.cursor);

        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        while buf.len() < max {
            let wanted = chunk.len().min(max - buf.len());
            let read = std::io::Read::read(&mut reader, &mut chunk[..wanted])
                .map_err(|err| FsError::Corrupt(format!("NTFS read: {err}")))?;
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read]);
        }
        Ok(buf)
    }

    fn file_size(&mut self, path: &str) -> Option<u64> {
        let record = self.resolve(path).ok()?;
        let file = self.ntfs.file(&mut self.cursor, record).ok()?;
        if file.is_directory() {
            return None;
        }
        // `data_size` is the size the file occupies on disk; the logical size
        // is the length of the data attribute.
        let attribute = file.data(&mut self.cursor, "")?.ok()?;
        let attribute = attribute.to_attribute().ok()?;
        Some(attribute.value_length())
    }
}
