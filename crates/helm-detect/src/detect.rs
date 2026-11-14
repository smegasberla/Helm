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
//!
//! A Linux installation is often spread over two partitions: the root
//! filesystem holds `/etc/os-release` and the boot partition (which is often
//! the EFI System Partition itself) holds the kernel and the loader
//! configuration. Both are put together into a single system.

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
use crate::esp::{self, EspContents, EspLoader, LoaderKind};
use crate::fs;
use crate::gpt::read_gpt;
use crate::os::{linux, windows, LinuxProbe, WindowsProbe};

/// Sector size used for the LBA maths of the GPT.
const SECTOR_SIZE: u64 = 512;

/// An EFI System Partition that was found on a disk.
#[derive(Debug, Clone)]
struct EspCandidate {
    /// Index of the disk the partition belongs to.
    disk_index: u32,
    partition: PartitionInfo,
    contents: EspContents,
}

/// A partition that holds a part of a Linux installation.
#[derive(Debug, Clone)]
struct LinuxPartition {
    disk_index: u32,
    partition: PartitionInfo,
    probe: LinuxProbe,
    /// The partition holds `/etc/os-release`.
    is_root: bool,
    /// The partition holds the kernel images.
    is_boot: bool,
}

/// A partition that holds a Windows installation.
#[derive(Debug, Clone)]
struct WindowsPartition {
    disk_index: u32,
    partition: PartitionInfo,
    probe: WindowsProbe,
}

/// A Linux installation, possibly spread over two partitions.
#[derive(Debug, Clone)]
struct LinuxSystem {
    disk_index: u32,
    disk_label: String,
    /// The partition with `/etc/os-release`.
    root: Option<PartitionInfo>,
    /// The partition with the kernels.
    boot: Option<PartitionInfo>,
    probe: LinuxProbe,
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
    let mut linux_partitions: Vec<LinuxPartition> = Vec::new();
    let mut windows_partitions: Vec<WindowsPartition> = Vec::new();
    let mut disk_labels: Vec<String> = Vec::new();

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
                disk_labels.push(raw.label());
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
        disk_labels.push(disk_label.clone());
        disk_infos.push(disk::disk_info(index as u32, raw, partitions.clone()));

        // Inspect the partitions.
        for partition in &partitions {
            let view = partition_device(&device, partition);
            let kind = partition.fs;
            if !kind.is_readable() {
                continue;
            }
            let Some(mut volume) = open_volume(&view, kind) else {
                continue;
            };

            match kind {
                helm_protocol::FileSystemKind::Fat12
                | helm_protocol::FileSystemKind::Fat16
                | helm_protocol::FileSystemKind::Fat32 => {
                    let contents = esp::scan_esp(volume.as_mut());
                    if contents.has_efi_dir {
                        esp_candidates.push(EspCandidate {
                            disk_index: index as u32,
                            partition: partition.clone(),
                            contents,
                        });
                    }
                    // The boot partition of a Linux system is often the EFI
                    // System Partition itself.
                    let probe = linux::probe(volume.as_mut());
                    if probe.has_kernels() {
                        linux_partitions.push(LinuxPartition {
                            disk_index: index as u32,
                            partition: partition.clone(),
                            probe,
                            is_root: false,
                            is_boot: true,
                        });
                    }
                }
                helm_protocol::FileSystemKind::Ext2
                | helm_protocol::FileSystemKind::Ext3
                | helm_protocol::FileSystemKind::Ext4
                | helm_protocol::FileSystemKind::Btrfs
                | helm_protocol::FileSystemKind::Xfs => {
                    let probe = linux::probe(volume.as_mut());
                    if probe.looks_like_linux() || probe.has_kernels() {
                        linux_partitions.push(LinuxPartition {
                            disk_index: index as u32,
                            partition: partition.clone(),
                            is_root: probe.has_etc,
                            is_boot: probe.has_kernels(),
                            probe,
                        });
                    }
                }
                helm_protocol::FileSystemKind::Ntfs => {
                    let probe = windows::probe(volume.as_mut());
                    if probe.looks_like_windows() {
                        windows_partitions.push(WindowsPartition {
                            disk_index: index as u32,
                            partition: partition.clone(),
                            probe,
                        });
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

    // Put the Linux partitions together into systems.
    let systems = build_linux_systems(&linux_partitions, &disk_labels);

    // Build the list of operating systems.
    for system in &systems {
        let id = format!(
            "os-{}-{}",
            system.disk_index,
            system
                .root
                .as_ref()
                .or(system.boot.as_ref())
                .map(|partition| partition.number)
                .unwrap_or(0)
        );
        let mut evidence = Vec::new();
        evidence.push(format!(
            "{} on partition {}",
            system.probe.distro_name.as_deref().unwrap_or("Linux"),
            system
                .root
                .as_ref()
                .or(system.boot.as_ref())
                .map(|partition| partition.number)
                .unwrap_or(0)
        ));
        if !system.probe.kernels.is_empty() {
            evidence.push(format!("kernel: {}", system.probe.kernels.join(", ")));
        }
        if !system.probe.initrds.is_empty() {
            evidence.push(format!("initramfs: {}", system.probe.initrds.join(", ")));
        }
        if system.probe.grub {
            evidence.push("GRUB configuration found".into());
        }
        if !system.probe.systemd_boot_entries.is_empty() {
            evidence.push(format!(
                "systemd-boot entries: {}",
                system.probe.systemd_boot_entries.join(", ")
            ));
        }
        if let Some(root) = &system.root {
            if root.fs == helm_protocol::FileSystemKind::Btrfs {
                evidence.push(format!(
                    "root filesystem: {} (btrfs)",
                    if root.label.is_empty() {
                        "partition"
                    } else {
                        &root.label
                    }
                ));
            }
        }

        let boot = linux_boot_target(system, &esp_candidates, &boot_entries);
        if let Some(target) = &boot {
            evidence.push(format!("boot loader: {}", target.label));
        }

        report.oses.push(DetectedOs {
            id,
            name: system.probe.display_name(),
            kind: OsKind::Linux,
            disk_index: system.disk_index,
            disk_label: system.disk_label.clone(),
            partition_number: system
                .root
                .as_ref()
                .or(system.boot.as_ref())
                .map(|partition| partition.number)
                .unwrap_or(0),
            partition_label: system
                .root
                .as_ref()
                .or(system.boot.as_ref())
                .map(|partition| partition.label.clone())
                .unwrap_or_default(),
            fs: system
                .root
                .as_ref()
                .or(system.boot.as_ref())
                .map(|partition| partition.fs)
                .unwrap_or(helm_protocol::FileSystemKind::Unknown),
            distro: system.probe.distro_id.clone(),
            icon: system.probe.icon.clone().or_else(|| Some("linux".into())),
            bootable: boot.is_some(),
            boot,
            evidence,
        });
    }

    for candidate in &windows_partitions {
        let id = format!("os-{}-{}", candidate.disk_index, candidate.partition.number);
        let mut evidence = vec![format!(
            "Windows on partition {}",
            candidate.partition.number
        )];
        if candidate.probe.winload {
            evidence.push("\\Windows\\System32\\winload.efi found".into());
        }
        if candidate.probe.ntoskrnl {
            evidence.push("\\Windows\\System32\\ntoskrnl.exe found".into());
        }
        if candidate.probe.bcd {
            evidence.push("BCD store found".into());
        }
        let boot = windows_boot_target(candidate.disk_index, &esp_candidates, &boot_entries);
        if let Some(target) = &boot {
            evidence.push(format!("boot loader: {}", target.label));
        }
        report.oses.push(DetectedOs {
            id,
            name: "Windows".to_string(),
            kind: OsKind::Windows,
            disk_index: candidate.disk_index,
            disk_label: disk_labels
                .get(candidate.disk_index as usize)
                .cloned()
                .unwrap_or_default(),
            partition_number: candidate.partition.number,
            partition_label: candidate.partition.label.clone(),
            fs: candidate.partition.fs,
            distro: None,
            icon: Some("windows".into()),
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
) -> Option<Box<dyn crate::fs::VolumeFs>> {
    match fs::open_filesystem(device.clone(), kind) {
        Ok(volume) => Some(volume),
        Err(err) => {
            log::debug!("cannot open {kind:?} volume: {err}");
            None
        }
    }
}

/// Put the Linux partitions of every disk together into systems.
///
/// A partition with the kernels belongs to the root partition of the same
/// disk when there is only one; otherwise it is a system of its own.
fn build_linux_systems(partitions: &[LinuxPartition], disk_labels: &[String]) -> Vec<LinuxSystem> {
    let mut systems: Vec<LinuxSystem> = Vec::new();

    let disk_indices: Vec<u32> = {
        let mut indices: Vec<u32> = partitions.iter().map(|part| part.disk_index).collect();
        indices.sort_unstable();
        indices.dedup();
        indices
    };

    for disk_index in disk_indices {
        let of_disk: Vec<&LinuxPartition> = partitions
            .iter()
            .filter(|part| part.disk_index == disk_index)
            .collect();
        let roots: Vec<&LinuxPartition> = of_disk
            .iter()
            .filter(|part| part.is_root)
            .copied()
            .collect();
        let boots: Vec<&LinuxPartition> = of_disk
            .iter()
            .filter(|part| part.is_boot)
            .copied()
            .collect();

        // A partition that is both (a root with the kernels in it).
        for part in &roots {
            if part.is_boot {
                systems.push(LinuxSystem {
                    disk_index,
                    disk_label: disk_labels
                        .get(disk_index as usize)
                        .cloned()
                        .unwrap_or_default(),
                    root: Some(part.partition.clone()),
                    boot: Some(part.partition.clone()),
                    probe: part.probe.clone(),
                });
            }
        }

        // A boot partition without its own root.
        for part in &boots {
            if part.is_root {
                continue;
            }
            let merged = roots.len() == 1 && !roots[0].is_boot;
            let probe = if merged {
                let mut probe = roots[0].probe.clone();
                probe.kernels = part.probe.kernels.clone();
                probe.initrds = part.probe.initrds.clone();
                probe.grub |= part.probe.grub;
                probe.systemd_boot_entries = part.probe.systemd_boot_entries.clone();
                probe
            } else {
                part.probe.clone()
            };
            systems.push(LinuxSystem {
                disk_index,
                disk_label: disk_labels
                    .get(disk_index as usize)
                    .cloned()
                    .unwrap_or_default(),
                root: if merged {
                    Some(roots[0].partition.clone())
                } else {
                    None
                },
                boot: Some(part.partition.clone()),
                probe,
            });
        }

        // A root without any kernel: it is probably not bootable, but it is
        // still a Linux installation.
        for part in &roots {
            if part.is_boot {
                continue;
            }
            if roots.len() == 1 && boots.len() == 1 {
                // It was merged above.
                continue;
            }
            systems.push(LinuxSystem {
                disk_index,
                disk_label: disk_labels
                    .get(disk_index as usize)
                    .cloned()
                    .unwrap_or_default(),
                root: Some(part.partition.clone()),
                boot: None,
                probe: part.probe.clone(),
            });
        }
    }

    systems
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

/// Build the boot target of a loader.
fn target_for_loader(
    loader: &EspLoader,
    esp: &EspCandidate,
    entries: &[FirmwareBootEntry],
) -> BootTarget {
    if let Some(entry) = entry_for_loader(entries, &loader.path) {
        return BootTarget {
            label: loader.path.clone(),
            mode: BootTargetMode::ExistingEntry {
                source_var: entry.var_name.clone(),
                load_option_hex: entry.device_path_hex.clone().unwrap_or_default(),
            },
        };
    }
    BootTarget {
        label: loader.path.clone(),
        mode: BootTargetMode::Synthetic {
            loader_path: loader.path.clone(),
            esp_partition_number: esp.partition.number,
            esp_unique_guid: esp.partition.unique_guid.clone(),
            esp_start_lba: esp.partition.start_lba,
            esp_sector_count: esp.partition.sector_count(),
        },
    }
}

/// Choose the loader of a Linux installation.
fn pick_loader<'a>(
    esp: &'a EspCandidate,
    probe: &LinuxProbe,
    boot_partition: Option<&PartitionInfo>,
) -> Option<&'a EspLoader> {
    // The loader of the distribution itself.
    if let Some(distro) = probe.distro_id.as_ref() {
        if let Some(loader) = esp.contents.loader_of_vendor(distro) {
            return Some(loader);
        }
    }
    // GRUB, when the installation has a GRUB configuration.
    if probe.grub {
        if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::Grub) {
            return Some(loader);
        }
    }
    // systemd-boot, when the installation has entries for it.
    if !probe.systemd_boot_entries.is_empty() {
        if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::SystemdBoot) {
            return Some(loader);
        }
    }
    // The boot partition is the EFI System Partition itself: its loader
    // belongs to this installation.
    if let Some(boot) = boot_partition {
        if boot.number == esp.partition.number {
            if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::SystemdBoot) {
                return Some(loader);
            }
            if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::Shim) {
                return Some(loader);
            }
            if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::Fallback) {
                return Some(loader);
            }
        }
    }
    // Whatever the partition holds.
    if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::SystemdBoot) {
        return Some(loader);
    }
    if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::Shim) {
        return Some(loader);
    }
    if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::Grub) {
        return Some(loader);
    }
    if let Some(loader) = esp.contents.loader_of_kind(LoaderKind::Fallback) {
        return Some(loader);
    }
    esp.contents.loaders.first()
}

/// Build the boot target of a Linux installation.
fn linux_boot_target(
    system: &LinuxSystem,
    esp_candidates: &[EspCandidate],
    entries: &[FirmwareBootEntry],
) -> Option<BootTarget> {
    // The EFI System Partitions of the disk of the installation.
    let candidates: Vec<&EspCandidate> = esp_candidates
        .iter()
        .filter(|esp| esp.disk_index == system.disk_index)
        .collect();
    if candidates.is_empty() {
        return None;
    }

    // The partition that holds the kernels comes first: its EFI System
    // Partition is the one of the installation.
    let boot_partition = system.boot.as_ref();
    if let Some(boot) = boot_partition {
        for esp in &candidates {
            if esp.partition.number == boot.number {
                if let Some(loader) = pick_loader(esp, &system.probe, Some(boot)) {
                    return Some(target_for_loader(loader, esp, entries));
                }
            }
        }
    }
    for esp in &candidates {
        if let Some(loader) = pick_loader(esp, &system.probe, boot_partition) {
            return Some(target_for_loader(loader, esp, entries));
        }
    }
    None
}

/// Build the boot target of a Windows installation.
fn windows_boot_target(
    disk_index: u32,
    esp_candidates: &[EspCandidate],
    entries: &[FirmwareBootEntry],
) -> Option<BootTarget> {
    // The EFI System Partition that holds the Windows boot manager.
    for esp in esp_candidates {
        if esp.disk_index != disk_index {
            continue;
        }
        let Some(loader) = esp.contents.loader_of_kind(LoaderKind::Windows) else {
            continue;
        };
        return Some(target_for_loader(loader, esp, entries));
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
