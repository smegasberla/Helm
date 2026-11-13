//! An in-memory filesystem used to test the operating system detection logic
//! without touching a real disk.

use std::collections::HashMap;

use crate::fs::{FsEntry, FsError, VolumeFs};

/// A filesystem that lives in memory.
#[derive(Debug, Default, Clone)]
pub struct MemoryVolume {
    files: HashMap<String, Vec<u8>>,
    dirs: HashMap<String, Vec<String>>,
}

impl MemoryVolume {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a file. The path is normalised (leading `/` removed).
    pub fn add_file(&mut self, path: &str, content: &[u8]) {
        let path = normalise(path);
        self.files.insert(path, content.to_vec());
    }

    /// Add a directory with the given children.
    pub fn add_dir(&mut self, path: &str, children: &[&str]) {
        let path = normalise(path);
        self.dirs
            .insert(path, children.iter().map(|c| c.to_string()).collect());
    }

    /// True when the volume holds the given file.
    pub fn has_file(&self, path: &str) -> bool {
        self.files.contains_key(&normalise(path))
    }
}

fn normalise(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

impl VolumeFs for MemoryVolume {
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, FsError> {
        let path = normalise(path);
        let children = self
            .dirs
            .get(&path)
            .ok_or_else(|| FsError::NotFound(path.clone()))?;
        let mut entries = Vec::new();
        for child in children {
            let full = if path.is_empty() {
                child.clone()
            } else {
                format!("{path}/{child}")
            };
            if let Some(content) = self.files.get(&full) {
                entries.push(FsEntry {
                    name: child.clone(),
                    is_dir: false,
                    size: content.len() as u64,
                });
            } else if self.dirs.contains_key(&full) {
                entries.push(FsEntry {
                    name: child.clone(),
                    is_dir: true,
                    size: 0,
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn read_file(&mut self, path: &str, max: usize) -> Result<Vec<u8>, FsError> {
        let path = normalise(path);
        let content = self
            .files
            .get(&path)
            .ok_or_else(|| FsError::NotFound(path.clone()))?;
        Ok(content.iter().copied().take(max).collect())
    }

    fn file_size(&mut self, path: &str) -> Option<u64> {
        self.files.get(&normalise(path)).map(|c| c.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_and_reads() {
        let mut volume = MemoryVolume::new();
        volume.add_file("/etc/os-release", b"NAME=Arch\n");
        volume.add_dir("/etc", &["os-release"]);
        volume.add_dir("/", &["etc"]);

        let entries = volume.list_dir("/etc").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "os-release");
        assert!(!entries[0].is_dir);
        assert_eq!(
            volume.read_file("/etc/os-release", 100).unwrap(),
            b"NAME=Arch\n"
        );
        assert_eq!(volume.file_size("/etc/os-release"), Some(10));
        assert!(volume.file_size("/etc/nope").is_none());
    }
}
