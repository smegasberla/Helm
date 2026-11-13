//! Detection of Windows installations.

use crate::fs::VolumeFs;
use crate::os::{find_case_insensitive, WindowsProbe};

/// Probe a partition for a Windows installation.
pub fn probe(volume: &mut dyn VolumeFs) -> WindowsProbe {
    let mut probe = WindowsProbe {
        winload: false,
        ntoskrnl: false,
        bcd: false,
    };

    // The UEFI boot manager of Windows.
    probe.winload =
        find_case_insensitive(volume, "/Windows/System32", &["winload.efi", "winload.exe"])
            .is_some();

    // The kernel of Windows: present on every installation.
    probe.ntoskrnl =
        find_case_insensitive(volume, "/Windows/System32", &["ntoskrnl.exe"]).is_some();

    // The boot configuration data store.
    probe.bcd = find_case_insensitive(volume, "/Boot", &["BCD"]).is_some();

    probe
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::memory::MemoryVolume;

    #[test]
    fn detects_a_windows_installation() {
        let mut volume = MemoryVolume::new();
        volume.add_file("/Windows/System32/winload.efi", b"efi");
        volume.add_file("/Windows/System32/ntoskrnl.exe", b"kernel");
        volume.add_file("/Boot/BCD", b"bcd");
        volume.add_dir("/Windows/System32", &["winload.efi", "ntoskrnl.exe"]);
        volume.add_dir("/Boot", &["BCD"]);
        volume.add_dir("/", &["Windows", "Boot"]);

        let probe = probe(&mut volume);
        assert!(probe.looks_like_windows());
        assert!(probe.is_bootable());
    }

    #[test]
    fn a_broken_windows_install_is_not_bootable() {
        let mut volume = MemoryVolume::new();
        volume.add_file("/Windows/System32/ntoskrnl.exe", b"kernel");
        volume.add_dir("/Windows/System32", &["ntoskrnl.exe"]);
        volume.add_dir("/", &["Windows"]);

        let probe = probe(&mut volume);
        assert!(probe.looks_like_windows());
        assert!(!probe.is_bootable());
    }

    #[test]
    fn an_empty_partition_is_not_windows() {
        let mut volume = MemoryVolume::new();
        let probe = probe(&mut volume);
        assert!(!probe.looks_like_windows());
    }
}
