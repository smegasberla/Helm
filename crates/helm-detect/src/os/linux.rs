//! Detection of Linux installations.

use crate::fs::VolumeFs;
use crate::os::{parse_os_release, LinuxProbe};

/// Directories that may hold the kernel and the initramfs.
const BOOT_DIRS: [&str; 3] = ["/boot", "/", "/usr/lib/modules"];

/// Names of the kernel images Helm accepts.
const KERNEL_PREFIXES: [&str; 3] = ["vmlinuz-", "vmlinux-", "vmlinuz."];

/// Names of the initramfs images Helm accepts.
const INITRD_PREFIXES: [&str; 4] = ["initramfs-", "initrd-", "initrd.img-", "initramfs"];

/// Probe a partition for a Linux installation.
pub fn probe(volume: &mut dyn VolumeFs) -> LinuxProbe {
    let mut probe = LinuxProbe::default();

    if let Ok(content) = volume.read_file("/etc/os-release", 64 * 1024) {
        let text = String::from_utf8_lossy(&content);
        let (name, id, icon) = parse_os_release(&text);
        probe.distro_name = name;
        probe.distro_id = id;
        probe.icon = icon;
        probe.has_etc = true;
    }

    for dir in BOOT_DIRS {
        let Ok(entries) = volume.list_dir(dir) else {
            continue;
        };
        for entry in entries {
            if entry.is_dir {
                continue;
            }
            let lower = entry.name.to_ascii_lowercase();
            if KERNEL_PREFIXES.iter().any(|p| lower.starts_with(p)) {
                probe.kernels.push(entry.name.clone());
            } else if INITRD_PREFIXES.iter().any(|p| lower.starts_with(p)) {
                probe.initrds.push(entry.name.clone());
            }
        }
    }
    probe.kernels.sort();
    probe.initrds.sort();

    // GRUB keeps its configuration in /boot/grub/grub.cfg.
    probe.grub = volume
        .list_dir("/boot/grub")
        .map(|entries| entries.iter().any(|entry| entry.name == "grub.cfg"))
        .unwrap_or(false);

    // systemd-boot keeps its entries in /boot/loader/entries.
    if let Ok(entries) = volume.list_dir("/boot/loader/entries") {
        probe.systemd_boot_entries = entries
            .into_iter()
            .filter(|entry| !entry.is_dir && entry.name.ends_with(".conf"))
            .map(|entry| entry.name)
            .collect();
    }
    // The `loader` directory may also live directly in /boot/loader.
    if probe.systemd_boot_entries.is_empty() {
        if let Ok(entries) = volume.list_dir("/loader/entries") {
            probe.systemd_boot_entries = entries
                .into_iter()
                .filter(|entry| !entry.is_dir && entry.name.ends_with(".conf"))
                .map(|entry| entry.name)
                .collect();
        }
    }

    probe
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::memory::MemoryVolume;

    fn arch_volume() -> MemoryVolume {
        let mut volume = MemoryVolume::new();
        volume.add_file(
            "/etc/os-release",
            b"NAME=\"Arch Linux\"\nID=arch\nPRETTY_NAME=\"Arch Linux\"\n",
        );
        volume.add_dir("/etc", &["os-release"]);
        volume.add_dir("/", &["etc", "boot"]);
        volume.add_file("/boot/vmlinuz-6.9.1-arch1-1", b"kernel");
        volume.add_file("/boot/initramfs-6.9.1-arch1-1.img", b"initrd");
        volume.add_file("/boot/grub/grub.cfg", b"menuentry 'Arch' {}\n");
        volume.add_dir(
            "/boot",
            &[
                "vmlinuz-6.9.1-arch1-1",
                "initramfs-6.9.1-arch1-1.img",
                "grub",
            ],
        );
        volume.add_dir("/boot/grub", &["grub.cfg"]);
        volume
    }

    #[test]
    fn detects_arch_linux() {
        let mut volume = arch_volume();
        let probe = probe(&mut volume);
        assert!(probe.looks_like_linux());
        assert_eq!(probe.distro_id.as_deref(), Some("arch"));
        assert_eq!(probe.distro_name.as_deref(), Some("Arch Linux"));
        assert_eq!(probe.kernels.len(), 1);
        assert_eq!(probe.initrds.len(), 1);
        assert!(probe.grub);
        assert!(probe.has_boot_loader());
        assert_eq!(probe.display_name(), "Arch Linux");
    }

    #[test]
    fn detects_systemd_boot() {
        let mut volume = MemoryVolume::new();
        volume.add_file("/etc/os-release", b"ID=fedora\nNAME=\"Fedora Linux\"\n");
        volume.add_dir("/etc", &["os-release"]);
        volume.add_dir("/boot", &["loader", "vmlinuz-6.9.0.fc40.x86_64"]);
        volume.add_dir("/boot/loader", &["entries"]);
        volume.add_dir("/boot/loader/entries", &["arch.conf"]);
        volume.add_file("/boot/loader/entries/arch.conf", b"title Arch\n");
        volume.add_file("/boot/vmlinuz-6.9.0.fc40.x86_64", b"kernel");

        let probe = probe(&mut volume);
        assert!(probe.looks_like_linux());
        assert!(!probe.grub);
        assert_eq!(probe.systemd_boot_entries.len(), 1);
        assert!(probe.has_boot_loader());
    }

    #[test]
    fn an_empty_partition_is_not_linux() {
        let mut volume = MemoryVolume::new();
        let probe = probe(&mut volume);
        assert!(!probe.looks_like_linux());
        assert!(!probe.has_boot_loader());
    }
}
