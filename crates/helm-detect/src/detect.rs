//! The detection pipeline.
//!
//! The pipeline is the same on every platform:
//!
//! 1. enumerate the physical disks,
//! 2. parse the GPT of every disk,
//! 3. identify the filesystem of every partition,
//! 4. look for operating systems on the partitions,
//! 5. look for the boot loaders on the EFI System Partitions,
//! 6. match everything with the boot entries of the firmware.

use std::sync::Arc;

use helm_boot::devicepath;
use helm_boot::store::decode_boot_entries;
use helm_boot::FirmwareStore;
use helm_protocol::{
    BootTarget, BootTargetMode, DetectedOs, DetectionReport, DiskInfo, FirmwareBootEntry, OsKind,
    PartitionInfo,
};

use crate::block::{BlockDevice, FileDevice, PartitionDevice};
use crate::disk::{self, RawDisk};
use crate::esp::{self, EspContents, LoaderKind};
use crate::fs::{self, VolumeFs};
use crate::gpt::read_gpt;
use crate::os::{linux, windows, LinuxProbe, WindowsProbe};

/// Sector size used for the LBA maths of the GPT.
const SECTOR_SIZE: u64 = 512;

/// An EFI System Partition that was found on a disk.
#[derive(Debug, Clone)]
struct EspCandidate {
    /// Index of the disk the partition belongs to.
    #[allow(dead_code)]
    disk_index: u32,
    partition: PartitionInfo,
    contents: EspContents,
}

/// A partition that holds an operating system.
#[derive(Debug, Clone)]
struct OsCandidate {
    disk_index: u32,
    disk_label: String,
    partition: PartitionInfo,
    linux: Option<LinuxProbe>,
    windows: Option<WindowsProbe>,
}

/// Run the whole detection pipeline on the disks of the machine.
pub fn detect(store: Option<&dyn FirmwareStore>) -> DetectionReport {
    detect_disks(disk::enumerate_disks(), store)
}

/// Run the detection pipeline on a given list of disks.
///
/// The helper uses this to look at a disk image instead of the real disks,
/// which is handy while developing.
pub fn detect_disks(raw_disks: Vec<RawDisk>, store: Option<&dyn FirmwareStore>) -> DetectionReport {
    let mut report = DetectionReport {
        uefi: helm_boot::running_under_uefi(),
        ..Default::default()
    };

    let mut disk_infos: Vec<DiskInfo> = Vec::new();
    let mut esp_candidates: Vec<EspCandidate> = Vec::new();
    let mut os_candidates: Vec<OsCandidate> = Vec::new();

    for (index, raw) in raw_disks.iter().enumerate() {
        let device = match FileDevice::open(&raw.path) {
            Ok(device) => Arc::new(device) as Arc<dyn BlockDevice>,
            Err(err) => {
                // The disk is still worth reporting, only its content cannot
                // be read without the right permissions.
                report
                    .warnings
                    .push(format!("cannot open {}: {err}", raw.path));
                disk_infos.push(disk::disk_info(index as u32, raw, Vec::new()));
                continue;
            }
        };

        let gpt = match read_gpt(device.as_ref()) {
            Ok(gpt) => Some(gpt),
            Err(err) => {
                if raw.size_bytes > SECTOR_SIZE * 2 {
                    report
                        .warnings
                        .push(format!("{}: no usable partition table ({err})", raw.path));
                }
                None
            }
        };

        let mut partitions = match &gpt {
            Some(gpt) => gpt.to_partition_infos(),
            None => Vec::new(),
        };

        // Identify the filesystem of every partition.
        for partition in &mut partitions {
            let view = partition_device(&device, partition);
            partition.fs = fs::detect_filesystem(&view);
        }

        let disk_label = raw.label();
        disk_infos.push(disk::disk_info(index as u32, raw, partitions.clone()));

        // Inspect the partitions.
        for partition in &partitions {
            let view = partition_device(&device, partition);
            match partition.fs {
                helm_protocol::FileSystemKind::Fat12
                | helm_protocol::FileSystemKind::Fat16
                | helm_protocol::FileSystemKind::Fat32 => {
                    if let Some(mut volume) = open_volume(&view, partition.fs) {
                        let contents = esp::scan_esp(volume.as_mut());
                        if contents.has_efi_dir {
                            esp_candidates.push(EspCandidate {
                                disk_index: index as u32,
                                partition: partition.clone(),
                                contents,
                            });
                        }
                    }
                }
                helm_protocol::FileSystemKind::Ext2
                | helm_protocol::FileSystemKind::Ext3
                | helm_protocol::FileSystemKind::Ext4 => {
                    if let Some(mut volume) = open_volume(&view, partition.fs) {
                        let probe = linux::probe(volume.as_mut());
                        if probe.looks_like_linux() {
                            os_candidates.push(OsCandidate {
                                disk_index: index as u32,
                                disk_label: disk_label.clone(),
                                partition: partition.clone(),
                                linux: Some(probe),
                                windows: None,
                            });
                        }
                    }
                }
                helm_protocol::FileSystemKind::Ntfs => {
                    if let Some(mut volume) = open_volume(&view, partition.fs) {
                        let probe = windows::probe(volume.as_mut());
                        if probe.looks_like_windows() {
                            os_candidates.push(OsCandidate {
                                disk_index: index as u32,
                                disk_label: disk_label.clone(),
                                partition: partition.clone(),
                                linux: None,
                                windows: Some(probe),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // The boot entries of the firmware.
    let boot_entries = match store {
        Some(store) => match decode_boot_entries(store) {
            Ok(entries) => entries,
            Err(err) => {
                report
                    .warnings
                    .push(format!("cannot read the firmware boot entries: {err}"));
                Vec::new()
            }
        },
        None => {
            report
                .warnings
                .push("the firmware variable store is not accessible".into());
            Vec::new()
        }
    };

    // Build the list of operating systems.
    for candidate in &os_candidates {
        let id = format!("os-{}-{}", candidate.disk_index, candidate.partition.number);
        let (kind, name, icon, evidence, boot) = if let Some(probe) = &candidate.linux {
            let mut evidence = Vec::new();
            evidence.push(format!(
                "{} on partition {}",
                probe.distro_name.as_deref().unwrap_or("Linux"),
                candidate.partition.number
            ));
            if !probe.kernels.is_empty() {
                evidence.push(format!("kernel: {}", probe.kernels.join(", ")));
            }
            if !probe.initrds.is_empty() {
                evidence.push(format!("initramfs: {}", probe.initrds.join(", ")));
            }
            if probe.grub {
                evidence.push("GRUB configuration found".into());
            }
            if !probe.systemd_boot_entries.is_empty() {
                evidence.push(format!(
                    "systemd-boot entries: {}",
                    probe.systemd_boot_entries.join(", ")
                ));
            }
            let boot = linux_boot_target(candidate, &esp_candidates, &boot_entries);
            if let Some(target) = &boot {
                evidence.push(format!("boot loader: {}", target.label));
            }
            (
                OsKind::Linux,
                probe.display_name(),
                probe.icon.clone().or_else(|| Some("linux".into())),
                evidence,
                boot,
            )
        } else if let Some(probe) = &candidate.windows {
            let mut evidence = Vec::new();
            evidence.push(format!(
                "Windows on partition {}",
                candidate.partition.number
            ));
            if probe.winload {
                evidence.push("\\Windows\\System32\\winload.efi found".into());
            }
            if probe.ntoskrnl {
                evidence.push("\\Windows\\System32\\ntoskrnl.exe found".into());
            }
            if probe.bcd {
                evidence.push("BCD store found".into());
            }
            let boot = windows_boot_target(&esp_candidates, &boot_entries);
            if let Some(target) = &boot {
                evidence.push(format!("boot loader: {}", target.label));
            }
            (
                OsKind::Windows,
                "Windows".to_string(),
                Some("windows".into()),
                evidence,
                boot,
            )
        } else {
            continue;
        };

        report.oses.push(DetectedOs {
            id,
            name,
            kind,
            disk_index: candidate.disk_index,
            disk_label: candidate.disk_label.clone(),
            partition_number: candidate.partition.number,
            partition_label: candidate.partition.label.clone(),
            fs: candidate.partition.fs,
            distro: candidate
                .linux
                .as_ref()
                .and_then(|probe| probe.distro_id.clone()),
            icon,
            bootable: boot.is_some(),
            boot,
            evidence,
        });
    }

    report.disks = disk_infos;
    report.boot_entries = boot_entries;
    report
}

/// Build a view on a partition of a disk.
fn partition_device(device: &Arc<dyn BlockDevice>, partition: &PartitionInfo) -> PartitionDevice {
    PartitionDevice::new(
        device.clone(),
        partition.start_lba * SECTOR_SIZE,
        partition.size_bytes,
    )
}

/// Open the filesystem of a partition.
fn open_volume(
    device: &PartitionDevice,
    kind: helm_protocol::FileSystemKind,
) -> Option<Box<dyn VolumeFs>> {
    match fs::open_filesystem(device.clone(), kind) {
        Ok(volume) => Some(volume),
        Err(err) => {
            log::debug!("cannot open {kind:?} volume: {err}");
            None
        }
    }
}

/// Find the firmware entry that points at a loader.
fn entry_for_loader<'a>(
    entries: &'a [FirmwareBootEntry],
    loader_path: &str,
) -> Option<&'a FirmwareBootEntry> {
    entries.iter().find(|entry| {
        entry
            .loader_path
            .as_ref()
            .map(|path| path.eq_ignore_ascii_case(loader_path))
            .unwrap_or(false)
    })
}

/// Build the boot target of a Linux installation.
fn linux_boot_target(
    candidate: &OsCandidate,
    esp_candidates: &[EspCandidate],
    entries: &[FirmwareBootEntry],
) -> Option<BootTarget> {
    let probe = candidate.linux.as_ref()?;
    let distro_id = probe.distro_id.clone().unwrap_or_default();

    // Preferred loaders, in order: the one of the distribution, GRUB and
    // systemd-boot.
    let mut wanted: Vec<(String, Option<LoaderKind>)> = Vec::new();
    if !distro_id.is_empty() {
        wanted.push((distro_id.clone(), None));
    }
    wanted.push(("grub".into(), Some(LoaderKind::Grub)));
    wanted.push(("systemd".into(), Some(LoaderKind::SystemdBoot)));

    for (vendor, kind) in wanted {
        // A generic GRUB only helps when the installation really has a GRUB
        // configuration, the same goes for systemd-boot.
        if vendor == "grub" && !probe.grub {
            continue;
        }
        if vendor == "systemd" && probe.systemd_boot_entries.is_empty() {
            continue;
        }
        for esp in esp_candidates {
            let loader = match kind {
                Some(kind) => esp.contents.loader_of_kind(kind),
                None => esp.contents.loader_of_vendor(&vendor),
            };
            let Some(loader) = loader else { continue };
            if let Some(entry) = entry_for_loader(entries, &loader.path) {
                return Some(BootTarget {
                    label: loader.path.clone(),
                    mode: BootTargetMode::ExistingEntry {
                        source_var: entry.var_name.clone(),
                        load_option_hex: entry.device_path_hex.clone().unwrap_or_default(),
                    },
                });
            }
            return Some(BootTarget {
                label: loader.path.clone(),
                mode: BootTargetMode::Synthetic {
                    loader_path: loader.path.clone(),
                    esp_partition_number: esp.partition.number,
                    esp_unique_guid: esp.partition.unique_guid.clone(),
                    esp_start_lba: esp.partition.start_lba,
                    esp_sector_count: esp.partition.sector_count(),
                },
            });
        }
    }
    None
}

/// Build the boot target of a Windows installation.
fn windows_boot_target(
    esp_candidates: &[EspCandidate],
    entries: &[FirmwareBootEntry],
) -> Option<BootTarget> {
    // The ESP that holds the Windows boot manager.
    for esp in esp_candidates {
        let Some(loader) = esp.contents.loader_of_kind(LoaderKind::Windows) else {
            continue;
        };
        if let Some(entry) = entry_for_loader(entries, &loader.path) {
            return Some(BootTarget {
                label: loader.path.clone(),
                mode: BootTargetMode::ExistingEntry {
                    source_var: entry.var_name.clone(),
                    load_option_hex: entry.device_path_hex.clone().unwrap_or_default(),
                },
            });
        }
        return Some(BootTarget {
            label: loader.path.clone(),
            mode: BootTargetMode::Synthetic {
                loader_path: loader.path.clone(),
                esp_partition_number: esp.partition.number,
                esp_unique_guid: esp.partition.unique_guid.clone(),
                esp_start_lba: esp.partition.start_lba,
                esp_sector_count: esp.partition.sector_count(),
            },
        });
    }

    // Fall back to a firmware entry that points at a Windows boot manager,
    // even when the ESP itself could not be read.
    entries
        .iter()
        .find(|entry| {
            entry
                .loader_path
                .as_ref()
                .map(|path| path.to_ascii_lowercase().contains("microsoft"))
                .unwrap_or(false)
        })
        .map(|entry| BootTarget {
            label: entry
                .loader_path
                .clone()
                .unwrap_or_else(|| entry.description.clone()),
            mode: BootTargetMode::ExistingEntry {
                source_var: entry.var_name.clone(),
                load_option_hex: entry.device_path_hex.clone().unwrap_or_default(),
            },
        })
}

/// Decode the partition GUID of a boot entry, when it has one.
pub fn entry_partition_guid(entry: &FirmwareBootEntry) -> Option<String> {
    let hex = entry.device_path_hex.as_ref()?;
    let bytes = helm_boot::store::hex_decode(hex)?;
    devicepath::partition_guid_from_device_path(&bytes)
}
