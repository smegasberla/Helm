//! Switching the machine to another operating system on the next boot.
//!
//! The firmware is told about the new target through the one shot `BootNext`
//! variable: the next boot uses that entry instead of the regular
//! `BootOrder`. The variable is consumed by the firmware, so a failed boot
//! simply falls back to the normal boot order.

use helm_protocol::{BootTarget, BootTargetMode, FirmwareBootEntry};

use crate::devicepath;
use crate::store::{decode_boot_entries, hex_decode, FirmwareError, FirmwareStore};

/// Name of the one shot boot variable.
pub const BOOT_NEXT: &str = "BootNext";

/// High level operations on the firmware boot configuration.
pub struct BootSwitcher<S: FirmwareStore> {
    store: S,
}

impl<S: FirmwareStore> BootSwitcher<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// The firmware's own boot entries.
    pub fn entries(&self) -> Result<Vec<FirmwareBootEntry>, FirmwareError> {
        decode_boot_entries(&self.store)
    }

    /// The raw payload of the pending `BootNext` variable, if any.
    pub fn boot_next(&self) -> Result<Option<Vec<u8>>, FirmwareError> {
        self.store.read(BOOT_NEXT)
    }

    /// Remove a pending `BootNext` variable.
    pub fn clear_boot_next(&self) -> Result<bool, FirmwareError> {
        self.store.delete(BOOT_NEXT)
    }

    /// Point the firmware at `target` for the next boot.
    pub fn set_boot_next(&self, target: &BootTarget) -> Result<(), FirmwareError> {
        let payload = match &target.mode {
            BootTargetMode::ExistingEntry { source_var, .. } => {
                // The payload is read from the firmware instead of trusting
                // the copy the GUI sent: the firmware is the authority on
                // what a boot entry looks like.
                let data = self.store.read(source_var)?.ok_or_else(|| {
                    FirmwareError::NotFound(format!(
                        "boot entry {source_var} disappeared from the firmware"
                    ))
                })?;
                data
            }
            BootTargetMode::Synthetic {
                loader_path,
                esp_partition_number,
                esp_unique_guid,
                esp_start_lba,
                esp_sector_count,
            } => {
                let guid = devicepath::guid_from_str(esp_unique_guid).ok_or_else(|| {
                    FirmwareError::Other(format!("invalid partition GUID {esp_unique_guid}"))
                })?;
                let media = devicepath::build_media_node(
                    *esp_partition_number,
                    *esp_start_lba,
                    *esp_sector_count,
                    &guid,
                );
                let file = devicepath::build_file_node(loader_path).ok_or_else(|| {
                    FirmwareError::Other(format!("invalid loader path {loader_path}"))
                })?;
                let mut device_path = media;
                device_path.extend_from_slice(&file);
                device_path.extend_from_slice(&devicepath::build_end_node());
                devicepath::build_load_option(&device_path, &target.label)
            }
        };

        log::info!(
            "writing {BOOT_NEXT} ({} bytes) for {}",
            payload.len(),
            target.label
        );
        self.store.write(BOOT_NEXT, &payload)
    }
}

/// Decode the hex encoded `LOAD_OPTION` payload carried by the protocol.
pub fn decode_load_option_hex(hex: &str) -> Option<Vec<u8>> {
    hex_decode(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct MemoryStore {
        vars: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl MemoryStore {
        fn insert(&self, name: &str, data: Vec<u8>) {
            self.vars.lock().unwrap().insert(name.to_string(), data);
        }
    }

    impl FirmwareStore for MemoryStore {
        fn read(&self, name: &str) -> Result<Option<Vec<u8>>, FirmwareError> {
            Ok(self.vars.lock().unwrap().get(name).cloned())
        }

        fn write(&self, name: &str, data: &[u8]) -> Result<(), FirmwareError> {
            self.vars
                .lock()
                .unwrap()
                .insert(name.to_string(), data.to_vec());
            Ok(())
        }

        fn delete(&self, name: &str) -> Result<bool, FirmwareError> {
            Ok(self.vars.lock().unwrap().remove(name).is_some())
        }

        fn list(&self) -> Result<Vec<String>, FirmwareError> {
            Ok(self.vars.lock().unwrap().keys().cloned().collect())
        }

        fn is_uefi(&self) -> bool {
            true
        }
    }

    fn sample_entry() -> Vec<u8> {
        let media = devicepath::build_media_node(
            1,
            2048,
            614400,
            &devicepath::guid_from_str("54375b93-313e-8746-a1f8-e69ae456a9e5").unwrap(),
        );
        let file = devicepath::build_file_node("\\EFI\\BOOT\\BOOTX64.EFI").unwrap();
        let mut dp = media;
        dp.extend_from_slice(&file);
        dp.extend_from_slice(&devicepath::build_end_node());
        devicepath::build_load_option(&dp, "UEFI OS")
    }

    #[test]
    fn existing_entry_is_copied_verbatim() {
        let store = MemoryStore::default();
        store.insert("Boot0001", sample_entry());
        let switcher = BootSwitcher::new(Box::new(store));

        let target = BootTarget {
            label: "UEFI OS".into(),
            mode: BootTargetMode::ExistingEntry {
                source_var: "Boot0001".into(),
                load_option_hex: String::new(),
            },
        };
        switcher.set_boot_next(&target).unwrap();

        let pending = switcher.boot_next().unwrap().unwrap();
        assert_eq!(pending, sample_entry());
    }

    #[test]
    fn synthetic_entry_is_built() {
        let store = MemoryStore::default();
        let switcher = BootSwitcher::new(Box::new(store));

        let target = BootTarget {
            label: "\\EFI\\grub\\grubx64.efi".into(),
            mode: BootTargetMode::Synthetic {
                loader_path: "\\EFI\\grub\\grubx64.efi".into(),
                esp_partition_number: 1,
                esp_unique_guid: "54375b93-313e-8746-a1f8-e69ae456a9e5".into(),
                esp_start_lba: 2048,
                esp_sector_count: 614400,
            },
        };
        switcher.set_boot_next(&target).unwrap();

        let pending = switcher.boot_next().unwrap().unwrap();
        let option = devicepath::parse_load_option(&pending);
        assert_eq!(
            devicepath::loader_path_from_device_path(&option.device_path).unwrap(),
            "\\EFI\\grub\\grubx64.efi"
        );
        assert_eq!(
            devicepath::partition_guid_from_device_path(&option.device_path).unwrap(),
            "54375b93-313e-8746-a1f8-e69ae456a9e5"
        );
    }

    #[test]
    fn missing_source_entry_is_an_error() {
        let store = MemoryStore::default();
        let switcher = BootSwitcher::new(Box::new(store));
        let target = BootTarget {
            label: "gone".into(),
            mode: BootTargetMode::ExistingEntry {
                source_var: "Boot0099".into(),
                load_option_hex: String::new(),
            },
        };
        assert!(switcher.set_boot_next(&target).is_err());
    }

    #[test]
    fn boot_next_can_be_cleared() {
        let store = MemoryStore::default();
        store.insert(BOOT_NEXT, vec![1, 2, 3]);
        let switcher = BootSwitcher::new(Box::new(store));
        assert!(switcher.clear_boot_next().unwrap());
        assert!(switcher.boot_next().unwrap().is_none());
    }

    #[test]
    fn entries_are_decoded() {
        let store = MemoryStore::default();
        store.insert("Boot0001", sample_entry());
        let switcher = BootSwitcher::new(Box::new(store));
        let entries = switcher.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].var_name, "Boot0001");
        assert_eq!(
            entries[0].loader_path.as_deref(),
            Some("\\EFI\\BOOT\\BOOTX64.EFI")
        );
    }
}
