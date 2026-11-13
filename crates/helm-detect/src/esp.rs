//! Discovery of the boot loaders stored on EFI System Partitions.

use crate::fs::VolumeFs;

/// What kind of loader a file on the ESP is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderKind {
    /// The Windows boot manager (`bootmgfw.efi`).
    Windows,
    /// GRUB.
    Grub,
    /// A distribution shim (`shimx64.efi`).
    Shim,
    /// systemd-boot.
    SystemdBoot,
    /// The fallback loader of a removable medium (`BOOTX64.EFI`).
    Fallback,
    /// Something Helm does not know about.
    Unknown,
}

/// A boot loader found on an EFI System Partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EspLoader {
    /// Full path inside the ESP, e.g. `\EFI\grub\grubx64.efi`.
    pub path: String,
    /// The vendor directory, e.g. `grub`.
    pub vendor: String,
    /// The file name, e.g. `grubx64.efi`.
    pub file_name: String,
    pub kind: LoaderKind,
}

/// The content of an EFI System Partition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EspContents {
    /// The partition holds an `EFI` directory.
    pub has_efi_dir: bool,
    /// The loaders that were found.
    pub loaders: Vec<EspLoader>,
}

impl EspContents {
    /// The first loader of a given kind.
    pub fn loader_of_kind(&self, kind: LoaderKind) -> Option<&EspLoader> {
        self.loaders.iter().find(|loader| loader.kind == kind)
    }

    /// The loader of a vendor directory, e.g. `ubuntu`.
    ///
    /// When a vendor directory holds several loaders, the one the firmware
    /// should boot is returned: the shim comes first because it is the entry
    /// point of the secure boot chain, GRUB is the fallback.
    pub fn loader_of_vendor(&self, vendor: &str) -> Option<&EspLoader> {
        let candidates: Vec<&EspLoader> = self
            .loaders
            .iter()
            .filter(|loader| loader.vendor.eq_ignore_ascii_case(vendor))
            .collect();
        candidates
            .iter()
            .min_by_key(|loader| priority(loader.kind))
            .copied()
    }
}

/// The order in which the loaders of a vendor directory are preferred.
fn priority(kind: LoaderKind) -> u8 {
    match kind {
        LoaderKind::Shim => 0,
        LoaderKind::Windows => 1,
        LoaderKind::SystemdBoot => 2,
        LoaderKind::Grub => 3,
        LoaderKind::Fallback => 4,
        LoaderKind::Unknown => 5,
    }
}

/// Classify a loader from its vendor directory and file name.
pub fn classify(vendor: &str, file_name: &str) -> LoaderKind {
    let vendor_lower = vendor.to_ascii_lowercase();
    let file_lower = file_name.to_ascii_lowercase();
    if vendor_lower == "microsoft" && file_lower.starts_with("bootmgfw") {
        return LoaderKind::Windows;
    }
    if file_lower.starts_with("shim") {
        return LoaderKind::Shim;
    }
    if file_lower.starts_with("systemd-boot")
        || (vendor_lower == "systemd" && file_lower.starts_with("systemd"))
    {
        return LoaderKind::SystemdBoot;
    }
    if file_lower.starts_with("grub") || vendor_lower == "grub" {
        return LoaderKind::Grub;
    }
    if file_lower.starts_with("bootx64") || file_lower.starts_with("bootia32") {
        return LoaderKind::Fallback;
    }
    LoaderKind::Unknown
}

/// Scan an EFI System Partition for boot loaders.
pub fn scan_esp(volume: &mut dyn VolumeFs) -> EspContents {
    let mut contents = EspContents::default();
    let Ok(vendors) = volume.list_dir("/EFI") else {
        return contents;
    };
    contents.has_efi_dir = true;

    for vendor in vendors {
        if !vendor.is_dir {
            continue;
        }
        // The loaders do not all live directly in the vendor directory:
        // Windows keeps its boot manager in `EFI/Microsoft/Boot`.
        let mut stack = vec![(format!("/EFI/{}", vendor.name), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = volume.list_dir(&dir) else {
                continue;
            };
            for entry in entries {
                if entry.is_dir {
                    if depth < 2 {
                        stack.push((format!("{dir}/{}", entry.name), depth + 1));
                    }
                    continue;
                }
                if !entry.name.to_ascii_lowercase().ends_with(".efi") {
                    continue;
                }
                let relative = dir.trim_start_matches("/EFI/").replace('/', "\\");
                contents.loaders.push(EspLoader {
                    path: format!("\\EFI\\{}\\{}", relative, entry.name),
                    vendor: vendor.name.clone(),
                    kind: classify(&vendor.name, &entry.name),
                    file_name: entry.name.clone(),
                });
            }
        }
    }
    contents.loaders.sort_by(|a, b| {
        a.path
            .to_ascii_lowercase()
            .cmp(&b.path.to_ascii_lowercase())
    });
    contents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::memory::MemoryVolume;

    fn esp_volume() -> MemoryVolume {
        let mut volume = MemoryVolume::new();
        volume.add_dir("/EFI", &["Microsoft", "ubuntu", "Boot"]);
        volume.add_dir("/EFI/Microsoft", &["Boot"]);
        volume.add_dir("/EFI/Microsoft/Boot", &["bootmgfw.efi"]);
        volume.add_file("/EFI/Microsoft/Boot/bootmgfw.efi", b"efi");
        volume.add_dir("/EFI/ubuntu", &["shimx64.efi", "grubx64.efi"]);
        volume.add_file("/EFI/ubuntu/shimx64.efi", b"efi");
        volume.add_file("/EFI/ubuntu/grubx64.efi", b"efi");
        volume.add_dir("/EFI/Boot", &["BOOTX64.EFI"]);
        volume.add_file("/EFI/Boot/BOOTX64.EFI", b"efi");
        volume
    }

    #[test]
    fn finds_the_loaders() {
        let mut volume = esp_volume();
        let contents = scan_esp(&mut volume);
        assert!(contents.has_efi_dir);
        assert_eq!(contents.loaders.len(), 4);

        let windows = contents.loader_of_kind(LoaderKind::Windows).unwrap();
        assert_eq!(windows.path, "\\EFI\\Microsoft\\Boot\\bootmgfw.efi");

        let ubuntu = contents.loader_of_vendor("ubuntu").unwrap();
        assert_eq!(ubuntu.kind, LoaderKind::Shim);
        assert_eq!(ubuntu.path, "\\EFI\\ubuntu\\shimx64.efi");

        assert!(contents.loader_of_kind(LoaderKind::Fallback).is_some());
    }

    #[test]
    fn a_partition_without_efi_directory_is_not_an_esp() {
        let mut volume = MemoryVolume::new();
        volume.add_dir("/", &["Windows"]);
        let contents = scan_esp(&mut volume);
        assert!(!contents.has_efi_dir);
        assert!(contents.loaders.is_empty());
    }

    #[test]
    fn classification_rules() {
        assert_eq!(classify("Microsoft", "bootmgfw.efi"), LoaderKind::Windows);
        assert_eq!(classify("ubuntu", "shimx64.efi"), LoaderKind::Shim);
        assert_eq!(classify("grub", "grubx64.efi"), LoaderKind::Grub);
        assert_eq!(
            classify("systemd", "systemd-bootx64.efi"),
            LoaderKind::SystemdBoot
        );
        assert_eq!(classify("Boot", "BOOTX64.EFI"), LoaderKind::Fallback);
        assert_eq!(classify("weird", "thing.efi"), LoaderKind::Unknown);
    }
}
