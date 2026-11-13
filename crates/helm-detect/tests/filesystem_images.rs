//! Integration tests that run the filesystem readers against real filesystem
//! images.
//!
//! The images are built with the tools of the host (`mkfs.ext4`, `debugfs`,
//! `mkfs.vfat`, `mtools`). When a tool is missing the test is skipped instead
//! of failing, so the test suite still runs on a bare machine.

use std::path::{Path, PathBuf};
use std::process::Command;

use helm_detect::block::{FileDevice, PartitionDevice};
use helm_detect::esp::{scan_esp, LoaderKind};
use helm_detect::fs::{detect_filesystem, open_filesystem};
use helm_detect::os::{linux, windows};
use helm_protocol::FileSystemKind;

fn have(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn run(command: &mut Command) {
    let status = command
        .status()
        .unwrap_or_else(|err| panic!("cannot run {command:?}: {err}"));
    assert!(status.success(), "command failed: {command:?}");
}

/// A temporary directory that is removed when it is dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("helm-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build an ext4 image that looks like a Linux installation.
fn build_ext4_image(dir: &Path) -> PathBuf {
    let image = dir.join("linux.ext4");
    let source_os_release = dir.join("os-release");
    std::fs::write(
        &source_os_release,
        "NAME=\"Arch Linux\"\nID=arch\nPRETTY_NAME=\"Arch Linux\"\nICON=archlinux\n",
    )
    .unwrap();

    let kernel = dir.join("vmlinuz-6.9.1-arch1-1");
    std::fs::write(&kernel, b"not a real kernel").unwrap();
    let initrd = dir.join("initramfs-6.9.1-arch1-1.img");
    std::fs::write(&initrd, b"not a real initramfs").unwrap();
    let grub_cfg = dir.join("grub.cfg");
    std::fs::write(&grub_cfg, b"menuentry 'Arch Linux' { linux /vmlinuz }\n").unwrap();

    run(Command::new("mkfs.ext4")
        .args(["-q", "-F"])
        .arg(&image)
        .arg("24M"));
    for mkdir in ["/etc", "/boot", "/boot/grub"] {
        run(Command::new("debugfs")
            .arg("-w")
            .args(["-R", &format!("mkdir {mkdir}")])
            .arg(&image));
    }
    for (from, to) in [
        (&source_os_release, "/etc/os-release"),
        (&kernel, "/boot/vmlinuz-6.9.1-arch1-1"),
        (&initrd, "/boot/initramfs-6.9.1-arch1-1.img"),
        (&grub_cfg, "/boot/grub/grub.cfg"),
    ] {
        run(Command::new("debugfs")
            .arg("-w")
            .args(["-R", &format!("write {} {to}", from.display())])
            .arg(&image));
    }
    image
}

/// Build a FAT image that looks like an EFI System Partition.
fn build_fat_image(dir: &Path) -> PathBuf {
    let image = dir.join("esp.fat");
    let loader = dir.join("shimx64.efi");
    std::fs::write(&loader, b"not a real loader").unwrap();
    let windows_loader = dir.join("bootmgfw.efi");
    std::fs::write(&windows_loader, b"not a real boot manager").unwrap();

    // mkfs.vfat refuses to work on a file that does not exist yet.
    std::fs::File::create(&image)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    run(Command::new("mkfs.vfat").args(["-F", "32"]).arg(&image));
    for mkdir in [
        "::/EFI",
        "::/EFI/ubuntu",
        "::/EFI/Microsoft",
        "::/EFI/Microsoft/Boot",
    ] {
        run(Command::new("mmd").args(["-i"]).arg(&image).arg(mkdir));
    }
    for (from, to) in [
        (&loader, "::/EFI/ubuntu/shimx64.efi"),
        (&windows_loader, "::/EFI/Microsoft/Boot/bootmgfw.efi"),
    ] {
        run(Command::new("mcopy")
            .args(["-i"])
            .arg(&image)
            .arg(from)
            .arg(to));
    }
    image
}

#[test]
fn reads_a_real_ext4_linux_installation() {
    if !have("mkfs.ext4") || !have("debugfs") {
        eprintln!("skipping: mkfs.ext4 or debugfs is not available");
        return;
    }
    let dir = TempDir::new("ext4");
    let image = build_ext4_image(dir.path());

    let device = FileDevice::open(&image).unwrap();
    let kind = detect_filesystem(&device);
    assert_eq!(kind, FileSystemKind::Ext4);

    let partition = PartitionDevice::new(std::sync::Arc::new(device), 0, 24 * 1024 * 1024);
    let mut volume = open_filesystem(partition, kind).expect("open ext4 volume");

    // The probe has to find the distribution, the kernel and the loader.
    let probe = linux::probe(volume.as_mut());
    assert!(probe.looks_like_linux(), "probe: {probe:?}");
    assert_eq!(probe.distro_id.as_deref(), Some("arch"));
    assert_eq!(probe.distro_name.as_deref(), Some("Arch Linux"));
    assert_eq!(probe.kernels, vec!["vmlinuz-6.9.1-arch1-1".to_string()]);
    assert_eq!(probe.initrds.len(), 1);
    assert!(probe.grub);
    assert!(probe.has_boot_loader());

    // The raw file access has to work as well.
    let content = volume.read_file("/etc/os-release", 4096).unwrap();
    assert!(String::from_utf8_lossy(&content).contains("ID=arch"));
    assert_eq!(volume.file_size("/boot/grub/grub.cfg"), Some(42));

    // The directory listing has to contain the kernel.
    let boot = volume.list_dir("/boot").unwrap();
    assert!(boot
        .iter()
        .any(|entry| entry.name == "vmlinuz-6.9.1-arch1-1"));
    assert!(boot
        .iter()
        .any(|entry| entry.is_dir && entry.name == "grub"));
}

#[test]
fn reads_a_real_esp() {
    if !have("mkfs.vfat") || !have("mcopy") || !have("mmd") {
        eprintln!("skipping: mkfs.vfat or mtools is not available");
        return;
    }
    let dir = TempDir::new("fat");
    let image = build_fat_image(dir.path());

    let device = FileDevice::open(&image).unwrap();
    let kind = detect_filesystem(&device);
    assert_eq!(kind, FileSystemKind::Fat32);

    let partition = PartitionDevice::new(std::sync::Arc::new(device), 0, 32 * 1024 * 1024);
    let mut volume = open_filesystem(partition, kind).expect("open FAT volume");

    let contents = scan_esp(volume.as_mut());
    assert!(contents.has_efi_dir);
    assert_eq!(contents.loaders.len(), 2);

    let shim = contents.loader_of_vendor("ubuntu").expect("shim");
    assert_eq!(shim.path, "\\EFI\\ubuntu\\shimx64.efi");
    assert_eq!(shim.kind, LoaderKind::Shim);

    let windows = contents
        .loader_of_kind(LoaderKind::Windows)
        .expect("windows loader");
    assert_eq!(windows.path, "\\EFI\\Microsoft\\Boot\\bootmgfw.efi");

    // The raw file access has to work as well.
    let content = volume.read_file("/EFI/ubuntu/shimx64.efi", 4096).unwrap();
    assert_eq!(content, b"not a real loader");
}

#[test]
fn an_ext4_image_is_not_a_windows_installation() {
    if !have("mkfs.ext4") || !have("debugfs") {
        eprintln!("skipping: mkfs.ext4 or debugfs is not available");
        return;
    }
    let dir = TempDir::new("ext4-win");
    let image = build_ext4_image(dir.path());
    let device = FileDevice::open(&image).unwrap();
    let kind = detect_filesystem(&device);
    let partition = PartitionDevice::new(std::sync::Arc::new(device), 0, 24 * 1024 * 1024);
    let mut volume = open_filesystem(partition, kind).unwrap();
    let probe = windows::probe(volume.as_mut());
    assert!(!probe.looks_like_windows());
}
