//! Enumeration of the physical disks of the machine.
//!
//! Only *whole* disks are enumerated: the partitions are read from the GPT
//! that lives on the disk itself. This is what makes it possible to see the
//! disks of an operating system Helm is not currently running under.

use std::io;

use helm_protocol::DiskInfo;

/// A physical disk as reported by the operating system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDisk {
    /// Path of the raw block device.
    pub path: String,
    pub model: String,
    pub serial: Option<String>,
    pub size_bytes: u64,
    /// Logical sector size in bytes.
    pub sector_size: u32,
    pub removable: bool,
}

impl RawDisk {
    /// A human readable label for the GUI.
    pub fn label(&self) -> String {
        let model = self.model.trim();
        if model.is_empty() {
            self.path.clone()
        } else {
            model.to_string()
        }
    }
}

/// Enumerate the physical disks of the machine.
pub fn enumerate_disks() -> Vec<RawDisk> {
    platform::enumerate_disks()
}

#[cfg(unix)]
mod platform {
    use std::fs;
    use std::path::Path;

    use super::RawDisk;

    /// Virtual devices that are not backed by a real disk.
    fn is_virtual(name: &str) -> bool {
        name.starts_with("zram")
            || name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("dm-")
            || name.starts_with("md")
            || name.starts_with("sr")
            || name.starts_with("fd")
            || name.starts_with("nbd")
    }

    fn read_trimmed(path: &Path) -> Option<String> {
        let text = fs::read_to_string(path).ok()?;
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    pub fn enumerate_disks() -> Vec<RawDisk> {
        let Ok(entries) = fs::read_dir("/sys/block") else {
            return Vec::new();
        };
        let mut disks = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_virtual(&name) {
                continue;
            }
            let base = entry.path();
            // Partitions show up in /sys/block as well; they are skipped.
            if base.join("partition").exists() {
                continue;
            }
            // A real disk has a `device` link pointing at the transport.
            if !base.join("device").exists() {
                continue;
            }
            let Some(sectors) = read_trimmed(&base.join("size")) else {
                continue;
            };
            let Ok(sectors) = sectors.parse::<u64>() else {
                continue;
            };
            let sector_size = read_trimmed(&base.join("queue/logical_block_size"))
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(512);
            let model = read_trimmed(&base.join("device/model"))
                .or_else(|| read_trimmed(&base.join("device/name")))
                .or_else(|| read_trimmed(&base.join("device/vendor")))
                .unwrap_or_default();
            let serial = read_trimmed(&base.join("device/serial"))
                .or_else(|| read_trimmed(&base.join("device/wwid")));
            let removable = read_trimmed(&base.join("removable"))
                .map(|s| s == "1")
                .unwrap_or(false);

            disks.push(RawDisk {
                path: format!("/dev/{name}"),
                model,
                serial,
                size_bytes: sectors * 512,
                sector_size,
                removable,
            });
        }
        disks.sort_by(|a, b| a.path.cmp(&b.path));
        disks
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::OpenOptions;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::Ioctl::{
        DeviceIoControl, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_QUERY_PROPERTY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl as RawDeviceIoControl;

    use super::RawDisk;

    #[repr(C)]
    struct StoragePropertyQuery {
        property_id: u32,
        query_type: u32,
        additional: [u8; 1],
    }

    #[repr(C)]
    struct StorageDeviceDescriptor {
        version: u32,
        size: u32,
        device_type: u8,
        device_type_modifier: u8,
        _reserved: [u8; 2],
        vendor_id_offset: u32,
        product_id_offset: u32,
        product_revision_offset: u32,
        serial_number_offset: u32,
        bus_type: u32,
        raw_properties_length: u32,
        raw_device_properties: [u8; 1],
    }

    #[repr(C)]
    struct DiskLengthInfo {
        length: i64,
    }

    fn read_descriptor_string(buffer: &[u8], offset: u32) -> Option<String> {
        let start = offset as usize;
        if start >= buffer.len() {
            return None;
        }
        let end = buffer[start..]
            .iter()
            .position(|b| *b == 0)
            .map(|p| start + p)
            .unwrap_or(buffer.len());
        let text = String::from_utf8_lossy(&buffer[start..end])
            .trim()
            .to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    pub fn enumerate_disks() -> Vec<RawDisk> {
        let mut disks = Vec::new();
        for index in 0..32u32 {
            let path = format!(r"\\.\PhysicalDrive{index}");
            let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_READ_ATTRIBUTES)
                .open(Path::new(&path))
            else {
                continue;
            };
            let handle = file.as_raw_handle() as HANDLE;

            // Disk size.
            let mut length_info = DiskLengthInfo { length: 0 };
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_DISK_GET_LENGTH_INFO,
                    std::ptr::null(),
                    0,
                    &mut length_info as *mut _ as *mut _,
                    std::mem::size_of::<DiskLengthInfo>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            let size_bytes = if ok != 0 && length_info.length > 0 {
                length_info.length as u64
            } else {
                continue;
            };

            // Model and serial number.
            let mut model = String::new();
            let mut serial = None;
            let query = StoragePropertyQuery {
                property_id: 0, // StorageDeviceProperty
                query_type: 0,  // PropertyStandardQuery
                additional: [0],
            };
            let mut descriptor = vec![0u8; 1024];
            let ok = unsafe {
                RawDeviceIoControl(
                    handle,
                    IOCTL_STORAGE_QUERY_PROPERTY,
                    &query as *const _ as *const _,
                    std::mem::size_of::<StoragePropertyQuery>() as u32,
                    descriptor.as_mut_ptr() as *mut _,
                    descriptor.len() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 && returned >= std::mem::size_of::<StorageDeviceDescriptor>() {
                let desc = StorageDeviceDescriptor {
                    version: 0,
                    size: 0,
                    device_type: 0,
                    device_type_modifier: 0,
                    _reserved: [0; 2],
                    vendor_id_offset: 0,
                    product_id_offset: 0,
                    product_revision_offset: 0,
                    serial_number_offset: 0,
                    bus_type: 0,
                    raw_properties_length: 0,
                    raw_device_properties: [0],
                };
                // SAFETY: the descriptor was filled by the driver; the
                // offsets are read from the buffer the driver filled in.
                let vendor = read_descriptor_string(&descriptor, desc.vendor_id_offset);
                let product = read_descriptor_string(&descriptor, desc.product_id_offset);
                let serial_number = read_descriptor_string(&descriptor, desc.serial_number_offset);
                model = [vendor, product]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                serial = serial_number;
            }

            disks.push(RawDisk {
                path,
                model,
                serial,
                size_bytes,
                sector_size: 512,
                removable: false,
            });
        }
        disks
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::RawDisk;

    pub fn enumerate_disks() -> Vec<RawDisk> {
        Vec::new()
    }
}

/// Turn a raw disk into the protocol representation.
pub fn disk_info(
    index: u32,
    raw: &RawDisk,
    partitions: Vec<helm_protocol::PartitionInfo>,
) -> DiskInfo {
    DiskInfo {
        index,
        path: raw.path.clone(),
        model: raw.model.trim().to_string(),
        serial: raw.serial.clone(),
        size_bytes: raw.size_bytes,
        sector_size: raw.sector_size,
        removable: raw.removable,
        gpt: !partitions.is_empty(),
        partitions,
    }
}

/// Errors raised while opening a disk.
#[derive(Debug, thiserror::Error)]
pub enum DiskError {
    #[error("cannot open {0}: {1}")]
    Open(String, io::Error),
}
