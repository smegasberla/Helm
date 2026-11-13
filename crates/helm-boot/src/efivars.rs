//! Access to the UEFI variable store through `efivarfs`.
//!
//! Layout of an efivarfs entry: the file content starts with the 4 byte
//! little endian variable attributes, followed by the variable payload.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::devicepath::EFI_GLOBAL_VARIABLE_GUID;
use crate::store::{FirmwareError, FirmwareStore, VAR_ATTRIBUTES};

/// Default mount point of `efivarfs`.
pub const EFIVARFS_MOUNT: &str = "/sys/firmware/efi/efivars";

/// A firmware variable store backed by `efivarfs`.
#[derive(Debug, Clone)]
pub struct EfiVarStore {
    dir: PathBuf,
}

impl EfiVarStore {
    /// Open the store. Fails when the machine did not boot through UEFI.
    pub fn open() -> Result<Self, FirmwareError> {
        Self::open_at(Path::new(EFIVARFS_MOUNT))
    }

    /// Open the store at an explicit location (used by the tests).
    pub fn open_at(dir: &Path) -> Result<Self, FirmwareError> {
        if !dir.is_dir() {
            return Err(FirmwareError::Unavailable(format!(
                "{} does not exist (the machine was not booted through UEFI)",
                dir.display()
            )));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn path_for(&self, name: &str) -> PathBuf {
        self.dir.join(format!(
            "{name}-{}",
            EFI_GLOBAL_VARIABLE_GUID.to_ascii_lowercase()
        ))
    }

    /// Locate the file backing a variable, tolerating firmware that stores
    /// global variables under a different GUID.
    fn find_path(&self, name: &str) -> Option<PathBuf> {
        let direct = self.path_for(name);
        if direct.exists() {
            return Some(direct);
        }
        let prefix = format!("{name}-");
        let entries = fs::read_dir(&self.dir).ok()?;
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.starts_with(&prefix) {
                return Some(entry.path());
            }
        }
        None
    }
}

impl FirmwareStore for EfiVarStore {
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, FirmwareError> {
        let Some(path) = self.find_path(name) else {
            return Ok(None);
        };
        let mut file = File::open(&path)?;
        let mut attrs = [0u8; 4];
        file.read_exact(&mut attrs)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Ok(Some(data))
    }

    fn write(&self, name: &str, data: &[u8]) -> Result<(), FirmwareError> {
        let path = self.path_for(name);
        let mut payload = Vec::with_capacity(4 + data.len());
        payload.extend_from_slice(&VAR_ATTRIBUTES.to_le_bytes());
        payload.extend_from_slice(data);

        // Overwriting an existing variable in place keeps the original
        // attributes (some firmware refuses to recreate a variable that was
        // created with authentication attributes).
        if path.exists() {
            match OpenOptions::new().write(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(&payload)?;
                    file.flush()?;
                    // Trim any leftovers from a longer previous value.
                    let expected = payload.len() as u64;
                    if let Ok(meta) = file.metadata() {
                        if meta.len() > expected {
                            let _ = file.set_len(expected);
                        }
                    }
                    return Ok(());
                }
                Err(err) => {
                    log::warn!("in-place update of {name} failed ({err}), recreating it");
                }
            }
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(&payload)?;
        file.flush()?;
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<bool, FirmwareError> {
        let Some(path) = self.find_path(name) else {
            return Ok(false);
        };
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(FirmwareError::Io(err)),
        }
    }

    fn list(&self) -> Result<Vec<String>, FirmwareError> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy().to_string();
            // The name of an efivarfs entry is "<name>-<guid>"; the GUID is
            // always 36 characters long.
            if file_name.len() > 37 {
                let (name, rest) = file_name.split_at(file_name.len() - 37);
                let guid = &rest[1..];
                if guid.len() == 36 && guid.contains('-') {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    fn is_uefi(&self) -> bool {
        true
    }
}
