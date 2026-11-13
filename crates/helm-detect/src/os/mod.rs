//! Operating system detection.
//!
//! The logic mirrors what `os-prober` and rEFInd do, but it is implemented
//! from scratch and works the same way on every platform: the partitions are
//! read directly, and an operating system is only reported when the files
//! that prove it can boot are really there.

pub mod linux;
pub mod windows;

use crate::fs::VolumeFs;

/// What was found on a partition that looks like a Linux installation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinuxProbe {
    /// `PRETTY_NAME` or `NAME` of `/etc/os-release`.
    pub distro_name: Option<String>,
    /// `ID` of `/etc/os-release`.
    pub distro_id: Option<String>,
    /// `ICON` of `/etc/os-release`.
    pub icon: Option<String>,
    /// Kernel images found in `/boot`.
    pub kernels: Vec<String>,
    /// Initramfs images found in `/boot`.
    pub initrds: Vec<String>,
    /// A GRUB configuration was found.
    pub grub: bool,
    /// systemd-boot entry files.
    pub systemd_boot_entries: Vec<String>,
    /// The partition holds an `/etc` directory.
    pub has_etc: bool,
}

impl LinuxProbe {
    /// True when the partition looks like a Linux installation.
    pub fn looks_like_linux(&self) -> bool {
        self.has_etc || !self.kernels.is_empty()
    }

    /// True when the installation can be booted through UEFI.
    pub fn has_boot_loader(&self) -> bool {
        self.grub || !self.systemd_boot_entries.is_empty()
    }

    /// A human readable name for the installation.
    pub fn display_name(&self) -> String {
        if let Some(name) = &self.distro_name {
            return name.clone();
        }
        if let Some(id) = &self.distro_id {
            return format!("Linux ({id})");
        }
        "Linux".to_string()
    }
}

/// What was found on a partition that looks like a Windows installation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsProbe {
    /// `\Windows\System32\winload.efi`, the UEFI boot manager.
    pub winload: bool,
    /// `\Windows\System32\ntoskrnl.exe`, the kernel.
    pub ntoskrnl: bool,
    /// A BCD store was found.
    pub bcd: bool,
}

impl WindowsProbe {
    /// True when the partition looks like a Windows installation.
    pub fn looks_like_windows(&self) -> bool {
        self.winload || self.ntoskrnl
    }

    /// True when the installation is complete enough to boot.
    pub fn is_bootable(&self) -> bool {
        self.winload && self.bcd
    }
}

/// Parse the shell style `key=value` format of `/etc/os-release`.
pub fn parse_os_release(content: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut name = None;
    let mut id = None;
    let mut icon = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value.trim_matches('"').trim_matches('\'');
        match key.trim() {
            "PRETTY_NAME" => name = Some(value.to_string()),
            "NAME" => {
                if name.is_none() {
                    name = Some(value.to_string());
                }
            }
            "ID" => id = Some(value.to_string()),
            "ICON" => icon = Some(value.to_string()),
            _ => {}
        }
    }
    (name, id, icon)
}

/// Look for a file in a directory, trying a few spellings.
pub fn find_case_insensitive(
    volume: &mut dyn VolumeFs,
    dir: &str,
    names: &[&str],
) -> Option<String> {
    let entries = volume.list_dir(dir).ok()?;
    for wanted in names {
        for entry in &entries {
            if entry.is_dir {
                continue;
            }
            if entry.name.eq_ignore_ascii_case(wanted) {
                return Some(entry.name.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_os_release() {
        let (name, id, icon) = parse_os_release(
            "NAME=\"Arch Linux\"\nID=arch\nPRETTY_NAME=\"Arch Linux\"\nICON=archlinux\n",
        );
        assert_eq!(name.as_deref(), Some("Arch Linux"));
        assert_eq!(id.as_deref(), Some("arch"));
        assert_eq!(icon.as_deref(), Some("archlinux"));
    }

    #[test]
    fn parses_minimal_os_release() {
        let (name, id, icon) = parse_os_release("ID=ubuntu\nVERSION_ID=\"24.04\"\n");
        assert_eq!(name, None);
        assert_eq!(id.as_deref(), Some("ubuntu"));
        assert_eq!(icon, None);
    }

    #[test]
    fn ignores_comments_and_junk() {
        let (name, id, _) = parse_os_release("# a comment\n\nBROKEN\nID=void\n");
        assert_eq!(name, None);
        assert_eq!(id.as_deref(), Some("void"));
    }
}
