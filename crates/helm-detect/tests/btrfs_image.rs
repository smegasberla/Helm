//! Integration test for the btrfs reader, against a real btrfs image that is
//! built by `mkfs.btrfs --rootdir` (no mounting needed).

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use common::{have, run, TempDir};
use helm_detect::block::{BlockDevice, FileDevice, PartitionDevice};
use helm_detect::fs::{btrfs, detect_filesystem, open_filesystem};
use helm_detect::os::linux;
use helm_protocol::FileSystemKind;

/// Build a btrfs image that looks like a Linux root filesystem.
fn build_btrfs_image(dir: &Path) -> Option<std::path::PathBuf> {
    if !have("mkfs.btrfs") {
        return None;
    }
    let root = dir.join("root");
    for directory in ["/etc", "/boot", "/boot/grub", "/boot/loader/entries"] {
        std::fs::create_dir_all(root.join(directory.trim_start_matches('/'))).unwrap();
    }
    std::fs::write(
        root.join("etc/os-release"),
        "NAME=\"Arch Linux\"\nID=arch\nPRETTY_NAME=\"Arch Linux\"\nICON=archlinux\n",
    )
    .unwrap();
    std::fs::write(root.join("boot/vmlinuz-6.9.1-arch1-1"), b"kernel").unwrap();
    std::fs::write(root.join("boot/initramfs-6.9.1-arch1-1.img"), b"initrd").unwrap();
    std::fs::write(
        root.join("boot/grub/grub.cfg"),
        b"menuentry 'Arch Linux' { linux /vmlinuz }\n",
    )
    .unwrap();

    let image = dir.join("linux.btrfs");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    run(Command::new("mkfs.btrfs")
        .args(["-q", "--rootdir"])
        .arg(&root)
        .arg(&image));
    Some(image)
}

#[test]
fn reads_a_real_btrfs_root_filesystem() {
    let dir = TempDir::new("btrfs");
    let Some(image) = build_btrfs_image(dir.path()) else {
        eprintln!("skipping: mkfs.btrfs is not available");
        return;
    };

    let device = FileDevice::open(&image).expect("open the image");
    assert_eq!(detect_filesystem(&device), FileSystemKind::Btrfs);

    let size = device.size().unwrap();
    let partition = PartitionDevice::new(Arc::new(device), 0, size);
    let mut volume =
        open_filesystem(partition, FileSystemKind::Btrfs).expect("open the btrfs volume");

    // The probe has to find the distribution, the kernel and the loader.
    let probe = linux::probe(volume.as_mut());
    assert!(probe.looks_like_linux(), "probe: {probe:?}");
    assert_eq!(probe.distro_id.as_deref(), Some("arch"));
    assert_eq!(probe.distro_name.as_deref(), Some("Arch Linux"));
    assert_eq!(probe.kernels, vec!["vmlinuz-6.9.1-arch1-1".to_string()]);
    assert_eq!(probe.initrds.len(), 1);
    assert!(probe.grub);

    // The raw access has to work as well.
    let content = volume.read_file("/etc/os-release", 4096).unwrap();
    assert!(String::from_utf8_lossy(&content).contains("ID=arch"));
    assert_eq!(volume.file_size("/boot/vmlinuz-6.9.1-arch1-1"), Some(6));

    let boot = volume.list_dir("/boot").unwrap();
    assert!(boot
        .iter()
        .any(|entry| entry.name == "vmlinuz-6.9.1-arch1-1"));
    assert!(boot
        .iter()
        .any(|entry| entry.is_dir && entry.name == "grub"));
}

/// The reader can be constructed directly.
#[test]
fn the_btrfs_reader_is_public() {
    let _ = btrfs::BtrfsVolume::open;
}
