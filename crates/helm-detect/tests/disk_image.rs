//! The end to end test of the detection: a disk image with a GPT that holds
//! a Linux partition, a Windows partition and an EFI System Partition.

mod common;

use std::path::Path;

use common::{
    build_disk_image, build_ext4_image, build_fat_image, have, ntfs_fixture, PartitionSpec, TempDir,
};
use helm_detect::detect_disks;
use helm_detect::RawDisk;
use helm_protocol::{FileSystemKind, OsKind};

const ESP_TYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
const LINUX_TYPE: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
const WINDOWS_TYPE: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";

#[test]
fn detects_the_operating_systems_of_a_disk_image() {
    if !have("mkfs.ext4") || !have("debugfs") || !have("mkfs.vfat") || !have("mcopy") {
        eprintln!("skipping: the filesystem tools are not available");
        return;
    }

    let dir = TempDir::new("disk");
    let ext4 = build_ext4_image(dir.path());
    let fat = build_fat_image(dir.path());
    let ntfs = ntfs_fixture();

    // The layout of the disk:
    //   1: the EFI System Partition (FAT32)
    //   2: the Linux root filesystem (ext4)
    //   3: the Windows installation (NTFS)
    let disk = dir.path().join("disk.img");
    let partitions = vec![
        PartitionSpec {
            type_guid: ESP_TYPE.to_string(),
            unique_guid: "11111111-2222-3333-4444-555555555555".to_string(),
            label: "EFI System Partition".to_string(),
            first_lba: 2048,
            last_lba: 2048 + 64 * 1024 * 1024 / 512 - 1,
            content: fat,
        },
        PartitionSpec {
            type_guid: LINUX_TYPE.to_string(),
            unique_guid: "66666666-7777-8888-9999-aaaaaaaaaaaa".to_string(),
            label: "Arch Linux".to_string(),
            first_lba: 2048 + 64 * 1024 * 1024 / 512,
            last_lba: 2048 + 64 * 1024 * 1024 / 512 + 24 * 1024 * 1024 / 512 - 1,
            content: ext4,
        },
        PartitionSpec {
            type_guid: WINDOWS_TYPE.to_string(),
            unique_guid: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff".to_string(),
            label: "Windows".to_string(),
            first_lba: 2048 + (64 * 1024 * 1024 + 24 * 1024 * 1024) / 512,
            last_lba: 2048 + (64 * 1024 * 1024 + 24 * 1024 * 1024) / 512 + 12 * 1024 * 1024 / 512
                - 1,
            content: ntfs,
        },
    ];
    build_disk_image(&disk, &partitions);

    // The image can be kept for a manual test of the interface.
    if let Ok(target) = std::env::var("HELM_KEEP_IMAGE") {
        std::fs::copy(&disk, &target).expect("keep the disk image");
        eprintln!("the disk image was written to {target}");
    }

    let raw = RawDisk {
        path: disk.to_string_lossy().to_string(),
        model: "Test Disk".to_string(),
        serial: None,
        size_bytes: std::fs::metadata(&disk).unwrap().len(),
        sector_size: 512,
        removable: false,
    };

    let report = detect_disks(vec![raw], None);

    // The disk and its partitions have to be there.
    assert_eq!(report.disks.len(), 1);
    let disk_info = &report.disks[0];
    assert!(disk_info.gpt);
    assert_eq!(disk_info.partitions.len(), 3);
    assert!(disk_info.partitions[0].is_esp);
    assert_eq!(
        disk_info.partitions[0].fs,
        FileSystemKind::Fat32,
        "the ESP was not recognised"
    );
    assert_eq!(disk_info.partitions[1].fs, FileSystemKind::Ext4);
    assert_eq!(disk_info.partitions[2].fs, FileSystemKind::Ntfs);

    // Two operating systems have to be found.
    assert_eq!(report.oses.len(), 2, "report: {report:?}");

    let linux = report
        .oses
        .iter()
        .find(|os| os.kind == OsKind::Linux)
        .expect("the Linux installation was not found");
    assert_eq!(linux.name, "Arch Linux");
    assert_eq!(linux.distro.as_deref(), Some("arch"));
    assert_eq!(linux.partition_number, 2);
    assert!(linux.bootable);
    // The GRUB loader of the ESP is the boot target.
    let target = linux.boot.as_ref().expect("no boot target");
    assert!(
        target.label.contains("grub") || target.label.contains("shim"),
        "unexpected boot target: {target:?}"
    );

    let windows = report
        .oses
        .iter()
        .find(|os| os.kind == OsKind::Windows)
        .expect("the Windows installation was not found");
    assert_eq!(windows.partition_number, 3);
    assert!(windows.bootable);
    let target = windows.boot.as_ref().expect("no boot target");
    assert_eq!(target.label, "\\EFI\\Microsoft\\Boot\\bootmgfw.efi");
}

#[test]
fn the_file_system_tools_are_available() {
    // The test above only runs when the tools are there; this test makes the
    // skip visible in the output.
    assert!(
        have("mkfs.ext4") || !Path::new("/nonexistent").exists(),
        "mkfs.ext4 is missing"
    );
}
