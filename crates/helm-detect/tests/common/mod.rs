//! Helpers that build the filesystem and disk images used by the tests.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A temporary directory that is removed when it is dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("helm-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the temporary directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// True when a tool is available.
pub fn have(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Run a command and panic when it fails.
pub fn run(command: &mut Command) {
    let status = command
        .status()
        .unwrap_or_else(|err| panic!("cannot run {command:?}: {err}"));
    assert!(status.success(), "the command failed: {command:?}");
}

/// Build an ext4 image that looks like a Linux installation.
pub fn build_ext4_image(dir: &Path) -> PathBuf {
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
pub fn build_fat_image(dir: &Path) -> PathBuf {
    let image = dir.join("esp.fat");
    let loader = dir.join("shimx64.efi");
    std::fs::write(&loader, b"not a real loader").unwrap();
    let windows_loader = dir.join("bootmgfw.efi");
    std::fs::write(&windows_loader, b"not a real boot manager").unwrap();
    let grub_loader = dir.join("grubx64.efi");
    std::fs::write(&grub_loader, b"not a real grub").unwrap();

    // mkfs.vfat refuses to work on a file that does not exist yet.
    std::fs::File::create(&image)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    run(Command::new("mkfs.vfat").args(["-F", "32"]).arg(&image));
    for mkdir in [
        "::/EFI",
        "::/EFI/ubuntu",
        "::/EFI/grub",
        "::/EFI/Microsoft",
        "::/EFI/Microsoft/Boot",
    ] {
        run(Command::new("mmd").args(["-i"]).arg(&image).arg(mkdir));
    }
    for (from, to) in [
        (&loader, "::/EFI/ubuntu/shimx64.efi"),
        (&grub_loader, "::/EFI/grub/grubx64.efi"),
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

/// The NTFS fixture that holds a Windows installation.
pub fn ntfs_fixture() -> PathBuf {
    let path = std::env::temp_dir().join(format!("helm-ntfs-{}.img", std::process::id()));
    let compressed = std::fs::File::open(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ntfs-windows.img.gz"),
    )
    .expect("open the fixture");
    let mut decoder = flate2::read::GzDecoder::new(compressed);
    let mut data = Vec::new();
    decoder.read_to_end(&mut data).expect("decompress");
    std::fs::write(&path, data).expect("write the fixture");
    path
}

/// A partition of the disk image that is built by [`build_disk_image`].
pub struct PartitionSpec {
    pub type_guid: String,
    pub unique_guid: String,
    pub label: String,
    pub first_lba: u64,
    pub last_lba: u64,
    /// The file with the content of the partition.
    pub content: PathBuf,
}

/// Build a GPT disk image that holds the given partitions.
pub fn build_disk_image(path: &Path, partitions: &[PartitionSpec]) {
    let sector_size = 512u64;
    let total_sectors = partitions.iter().map(|p| p.last_lba).max().unwrap_or(2048) + 1;

    let mut image = vec![0u8; (total_sectors * sector_size) as usize];

    // Protective MBR.
    image[440..444].copy_from_slice(&1u32.to_le_bytes());
    image[444..448].copy_from_slice(&1u32.to_le_bytes());
    image[448..452].copy_from_slice(&(total_sectors as u32 - 1).to_le_bytes());
    image[450] = 0xEE;

    // Copy the content of the partitions.
    for partition in partitions {
        let content = std::fs::read(&partition.content).expect("read the partition content");
        let offset = (partition.first_lba * sector_size) as usize;
        let end = offset + content.len();
        assert!(
            end <= image.len(),
            "the partition does not fit in the image"
        );
        image[offset..end].copy_from_slice(&content);
    }

    // The partition entries.
    let entry_count = 32usize;
    let entry_size = 128usize;
    let entries_lba = 2u64;
    let mut entries = vec![0u8; entry_count * entry_size];
    for (index, partition) in partitions.iter().enumerate() {
        let offset = index * entry_size;
        entries[offset..offset + 16].copy_from_slice(&guid_bytes(&partition.type_guid));
        entries[offset + 16..offset + 32].copy_from_slice(&guid_bytes(&partition.unique_guid));
        entries[offset + 32..offset + 40].copy_from_slice(&partition.first_lba.to_le_bytes());
        entries[offset + 40..offset + 48].copy_from_slice(&partition.last_lba.to_le_bytes());
        let units: Vec<u16> = partition.label.encode_utf16().collect();
        for (i, unit) in units.iter().enumerate() {
            entries[offset + 56 + i * 2..offset + 58 + i * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }

    // The GPT header.
    let mut header = vec![0u8; 92];
    header[..8].copy_from_slice(b"EFI PART");
    header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    header[12..16].copy_from_slice(&92u32.to_le_bytes());
    header[16..20].copy_from_slice(&[0, 0, 0, 0]);
    header[24..32].copy_from_slice(&1u64.to_le_bytes());
    header[32..40].copy_from_slice(&(total_sectors - 1).to_le_bytes());
    header[40..48].copy_from_slice(&(entries_lba + 1).to_le_bytes());
    header[48..56].copy_from_slice(&(total_sectors - 1 - 1).to_le_bytes());
    header[56..72].copy_from_slice(&guid_bytes("12345678-1234-5678-9abc-def012345678"));
    header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    header[80..84].copy_from_slice(&(entry_count as u32).to_le_bytes());
    header[84..88].copy_from_slice(&(entry_size as u32).to_le_bytes());
    header[88..92].copy_from_slice(&helm_detect::gpt::crc32(&entries).to_le_bytes());
    let crc = helm_detect::gpt::crc32(&header);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
    image[sector_size as usize..sector_size as usize + 92].copy_from_slice(&header);

    let entries_offset = (entries_lba * sector_size) as usize;
    image[entries_offset..entries_offset + entries.len()].copy_from_slice(&entries);

    std::fs::write(path, image).expect("write the disk image");
}

/// The mixed endian byte representation of a GUID.
fn guid_bytes(guid: &str) -> [u8; 16] {
    let hex: String = guid.chars().filter(|c| *c != '-').collect();
    let mut raw = [0u8; 16];
    for (index, byte) in raw.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("a valid GUID");
    }
    let mut out = [0u8; 16];
    out[0] = raw[3];
    out[1] = raw[2];
    out[2] = raw[1];
    out[3] = raw[0];
    out[4] = raw[5];
    out[5] = raw[4];
    out[6] = raw[7];
    out[7] = raw[6];
    out[8..16].copy_from_slice(&raw[8..16]);
    out
}
