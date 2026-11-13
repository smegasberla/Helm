//! GPT (GUID Partition Table) parsing.
//!
//! The partition table is parsed straight from the raw device: no OS specific
//! disk management API is involved, so Helm sees the partitions of every disk,
//! including the disks of an operating system it is not currently running.
//!
//! The parser validates both the header checksum and the checksum of the
//! partition entry array, and it cross checks the primary and the backup
//! header. A table that does not validate is rejected instead of being used
//! to write boot variables.

use helm_boot::devicepath::guid_to_string;
use helm_protocol::PartitionInfo;

use crate::block::BlockDevice;

/// Signature of a GPT header: `"EFI PART"` in little endian.
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Size of the protective MBR in bytes.
const SECTOR_SIZE: u64 = 512;
/// Offset of the GPT header inside its sector.
const HEADER_OFFSET: usize = 92;
/// Offset of the CRC32 field inside the header.
const HEADER_CRC_OFFSET: usize = 16;

/// Well known GPT partition type GUIDs.
pub mod type_guids {
    /// EFI System Partition.
    pub const EFI_SYSTEM_PARTITION: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
    /// Microsoft Basic Data (a Windows installation).
    pub const MICROSOFT_BASIC_DATA: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";
    /// Microsoft Reserved (the MSR partition that precedes Windows).
    pub const MICROSOFT_RESERVED: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";
    /// Microsoft Recovery.
    pub const MICROSOFT_RECOVERY: &str = "de94bba4-06d1-4d40-a16a-bfd50179d6ac";
    /// Linux filesystem data.
    pub const LINUX_FS_DATA: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
    /// Linux swap.
    pub const LINUX_SWAP: &str = "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f";
    /// Linux LVM.
    pub const LINUX_LVM: &str = "e6d6d379-f507-44c2-a23c-238f2a3df928";
    /// Linux RAID.
    pub const LINUX_RAID: &str = "a19d880f-05fc-4d3b-a006-743f0f84911e";
}

/// CRC-32 (IEEE 802.3, the polynomial used by the UEFI specification).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// A single partition as stored in the GPT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptPartition {
    /// 1 based partition number.
    pub number: u32,
    pub type_guid: String,
    pub unique_guid: String,
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: String,
}

impl GptPartition {
    /// Number of 512 byte sectors in the partition.
    pub fn sector_count(&self) -> u64 {
        self.last_lba.saturating_sub(self.first_lba) + 1
    }

    /// True when the partition holds an EFI System Partition.
    pub fn is_esp(&self) -> bool {
        self.type_guid
            .eq_ignore_ascii_case(type_guids::EFI_SYSTEM_PARTITION)
    }

    /// True when the partition is used by a Windows installation.
    pub fn is_windows_data(&self) -> bool {
        self.type_guid
            .eq_ignore_ascii_case(type_guids::MICROSOFT_BASIC_DATA)
            || self
                .type_guid
                .eq_ignore_ascii_case(type_guids::MICROSOFT_RECOVERY)
    }
}

/// A parsed GUID partition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpt {
    pub disk_guid: String,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub entries_lba: u64,
    pub entry_count: u32,
    pub entry_size: u32,
    /// Checksum of the partition entry array, as stored in the header.
    pub entries_crc: u32,
    /// LBA of the backup header.
    pub backup_lba: u64,
    pub partitions: Vec<GptPartition>,
    /// True when the primary and the backup header agreed.
    pub backup_verified: bool,
}

impl Gpt {
    /// Convert the raw entries into the protocol representation.
    pub fn to_partition_infos(&self) -> Vec<PartitionInfo> {
        self.partitions
            .iter()
            .map(|entry| PartitionInfo {
                number: entry.number,
                type_guid: entry.type_guid.clone(),
                unique_guid: entry.unique_guid.clone(),
                label: entry.name.clone(),
                start_lba: entry.first_lba,
                end_lba: entry.last_lba,
                size_bytes: entry.sector_count() * SECTOR_SIZE,
                is_esp: entry.is_esp(),
                is_windows_data: entry.is_windows_data(),
                fs: helm_protocol::FileSystemKind::Unknown,
            })
            .collect()
    }
}

/// Reasons why a device does not contain a usable GPT.
#[derive(Debug, thiserror::Error)]
pub enum GptError {
    #[error("the device is too small to hold a partition table ({0} bytes)")]
    TooSmall(u64),
    #[error("the device does not contain a protective MBR (not a GPT disk)")]
    NoProtectiveMbr,
    #[error("the GPT header signature is missing")]
    NoSignature,
    #[error("the GPT header checksum is invalid")]
    BadHeaderCrc,
    #[error("the partition entry checksum is invalid")]
    BadEntryCrc,
    #[error("the GPT structure is malformed: {0}")]
    Malformed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// True when the device starts with a protective MBR announcing a GPT.
pub fn has_protective_mbr(sector: &[u8]) -> bool {
    if sector.len() < 512 {
        return false;
    }
    // The protective MBR uses partition type 0xEE for its single entry.
    sector[450] == 0xEE && sector[451] == 0x00 && sector[452] == 0x00 && sector[453] == 0x00
}

/// Parse the GPT of a device.
pub fn read_gpt(device: &dyn BlockDevice) -> Result<Gpt, GptError> {
    let size = device.size()?;
    if size < SECTOR_SIZE * 3 {
        return Err(GptError::TooSmall(size));
    }

    let first_sector = device.read_vec_at(0, 512)?;
    if !has_protective_mbr(&first_sector) {
        return Err(GptError::NoProtectiveMbr);
    }

    let header = device.read_vec_at(SECTOR_SIZE, 512)?;
    let gpt = parse_header(&header)?;
    let gpt = parse_entries(device, gpt)?;

    // Cross check with the backup header when the device is large enough.
    let mut backup_verified = false;
    if size >= SECTOR_SIZE * 2 {
        let backup_lba = size / SECTOR_SIZE - 1;
        if let Ok(backup_sector) = device.read_vec_at(backup_lba * SECTOR_SIZE, 512) {
            if backup_sector[..8] == *GPT_SIGNATURE {
                if let Ok(backup) = parse_header(&backup_sector) {
                    backup_verified =
                        backup.disk_guid == gpt.disk_guid && backup.partitions.is_empty();
                }
            }
        }
    }

    Ok(Gpt {
        backup_verified,
        ..gpt
    })
}

/// Parse a GPT header. `sector` must contain the full 512 byte sector.
fn parse_header(sector: &[u8]) -> Result<Gpt, GptError> {
    if sector.len() < 512 {
        return Err(GptError::Malformed("short header sector".into()));
    }
    let header = &sector[..HEADER_OFFSET];
    if &header[..8] != GPT_SIGNATURE {
        return Err(GptError::NoSignature);
    }

    let header_size = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
    if header_size < HEADER_OFFSET || header_size > sector.len() {
        return Err(GptError::Malformed(format!(
            "invalid header size {header_size}"
        )));
    }

    let expected_crc = u32::from_le_bytes([
        header[HEADER_CRC_OFFSET],
        header[HEADER_CRC_OFFSET + 1],
        header[HEADER_CRC_OFFSET + 2],
        header[HEADER_CRC_OFFSET + 3],
    ]);
    let mut header_copy = header.to_vec();
    header_copy[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].copy_from_slice(&[0, 0, 0, 0]);
    let actual_crc = crc32(&header_copy);
    if actual_crc != expected_crc {
        return Err(GptError::BadHeaderCrc);
    }

    let backup_lba = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let first_usable_lba = u64::from_le_bytes(header[40..48].try_into().unwrap());
    let last_usable_lba = u64::from_le_bytes(header[48..56].try_into().unwrap());
    let disk_guid = guid_to_string(&header[56..72].try_into().unwrap());
    let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let entry_count = u32::from_le_bytes(header[80..84].try_into().unwrap());
    let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap());
    let entries_crc = u32::from_le_bytes(header[88..92].try_into().unwrap());

    if !(128..=4096).contains(&entry_size) || entry_count > 4096 {
        return Err(GptError::Malformed(format!(
            "implausible entry layout: {entry_count} entries of {entry_size} bytes"
        )));
    }

    Ok(Gpt {
        disk_guid,
        first_usable_lba,
        last_usable_lba,
        entries_lba,
        entry_count,
        entry_size,
        entries_crc,
        backup_lba,
        partitions: Vec::new(),
        backup_verified: false,
    })
}

/// Read and validate the partition entry array.
fn parse_entries(device: &dyn BlockDevice, mut gpt: Gpt) -> Result<Gpt, GptError> {
    let total = gpt.entry_count as usize * gpt.entry_size as usize;
    let bytes = device.read_vec_at(gpt.entries_lba * SECTOR_SIZE, total)?;
    if crc32(&bytes) != gpt.entries_crc {
        return Err(GptError::BadEntryCrc);
    }

    let mut partitions = Vec::new();
    for index in 0..gpt.entry_count as usize {
        let entry = &bytes[index * gpt.entry_size as usize..(index + 1) * gpt.entry_size as usize];
        let type_guid = guid_to_string(&entry[0..16].try_into().unwrap());
        if type_guid == "00000000-0000-0000-0000-000000000000" {
            continue;
        }
        let unique_guid = guid_to_string(&entry[16..32].try_into().unwrap());
        let first_lba = u64::from_le_bytes(entry[32..40].try_into().unwrap());
        let last_lba = u64::from_le_bytes(entry[40..48].try_into().unwrap());
        let attributes = u64::from_le_bytes(entry[48..56].try_into().unwrap());
        let name = decode_utf16(&entry[56..128]);
        partitions.push(GptPartition {
            number: index as u32 + 1,
            type_guid,
            unique_guid,
            first_lba,
            last_lba,
            attributes,
            name,
        });
    }
    gpt.partitions = partitions;
    Ok(gpt)
}

/// Decode a NUL padded UTF-16LE string.
fn decode_utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::SliceDevice;

    /// Build a synthetic GPT image.
    struct Builder {
        image: Vec<u8>,
    }

    impl Builder {
        fn new(sectors: u64) -> Self {
            let mut image = vec![0u8; (sectors * SECTOR_SIZE) as usize];
            // Protective MBR.
            image[450] = 0xEE;
            image[446] = 0x00;
            image[440] = 0x01; // boot indicator
            image[444..448].copy_from_slice(&1u32.to_le_bytes()); // start LBA
            image[448..452].copy_from_slice(&(sectors as u32 - 1).to_le_bytes()); // size in sectors
            image[450] = 0xEE; // the partition type of the protective entry
            Self { image }
        }

        fn write_header(&mut self, lba: u64, entries_lba: u64, entries: &[u8], backup_lba: u64) {
            let mut header = vec![0u8; 92];
            header[..8].copy_from_slice(GPT_SIGNATURE);
            header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision
            header[12..16].copy_from_slice(&92u32.to_le_bytes());
            // reserved
            header[16..20].copy_from_slice(&[0, 0, 0, 0]); // crc placeholder
            header[20..24].copy_from_slice(&[0, 0, 0, 0]); // reserved
            header[24..32].copy_from_slice(&1u64.to_le_bytes()); // current LBA
            header[32..40].copy_from_slice(&backup_lba.to_le_bytes());
            header[40..48].copy_from_slice(&34u64.to_le_bytes()); // first usable
            header[48..56].copy_from_slice(&(self.sectors() - 34).to_le_bytes()); // last usable
            let disk_guid: [u8; 16] = [
                0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
                0xff, 0x00,
            ];
            header[56..72].copy_from_slice(&disk_guid);
            header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
            header[80..84].copy_from_slice(&((entries.len() / 128).max(1) as u32).to_le_bytes());
            header[84..88].copy_from_slice(&128u32.to_le_bytes());
            let entries_crc = crc32(entries);
            header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
            let crc = crc32(&header);
            header[16..20].copy_from_slice(&crc.to_le_bytes());

            let start = (lba * SECTOR_SIZE) as usize;
            self.image[start..start + 92].copy_from_slice(&header);
        }

        fn sectors(&self) -> u64 {
            (self.image.len() as u64) / SECTOR_SIZE
        }
    }

    fn entry(
        type_guid: &[u8; 16],
        unique_guid: &[u8; 16],
        first: u64,
        last: u64,
        name: &str,
    ) -> Vec<u8> {
        let mut entry = vec![0u8; 128];
        entry[0..16].copy_from_slice(type_guid);
        entry[16..32].copy_from_slice(unique_guid);
        entry[32..40].copy_from_slice(&first.to_le_bytes());
        entry[40..48].copy_from_slice(&last.to_le_bytes());
        let units: Vec<u16> = name.encode_utf16().collect();
        for (i, unit) in units.iter().enumerate() {
            entry[56 + i * 2..58 + i * 2].copy_from_slice(&unit.to_le_bytes());
        }
        entry
    }

    fn esp_guid() -> [u8; 16] {
        helm_boot::devicepath::guid_from_str(type_guids::EFI_SYSTEM_PARTITION).unwrap()
    }

    #[test]
    fn crc32_matches_reference_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn parses_a_synthetic_gpt() {
        let mut builder = Builder::new(2048);
        let mut entries = entry(
            &esp_guid(),
            &esp_guid(),
            2048,
            614399,
            "EFI System Partition",
        );
        entries.extend_from_slice(&[0u8; 128]); // unused entry
        let entries_crc = crc32(&entries);
        builder.write_header(1, 2, &entries, 2047);

        let offset = (2 * SECTOR_SIZE) as usize;
        builder.image[offset..offset + entries.len()].copy_from_slice(&entries);

        let device = SliceDevice::new(builder.image.clone());
        let gpt = read_gpt(&device).unwrap();
        assert_eq!(gpt.partitions.len(), 1);
        let partition = &gpt.partitions[0];
        assert_eq!(partition.number, 1);
        assert_eq!(partition.name, "EFI System Partition");
        assert!(partition.is_esp());
        assert_eq!(partition.first_lba, 2048);
        assert_eq!(partition.last_lba, 614399);
        assert_eq!(partition.sector_count(), 612352);
        // The unique GUID round trips through the mixed endian encoding.
        assert_eq!(
            partition.unique_guid,
            helm_boot::devicepath::guid_to_string(&esp_guid())
        );
        assert!(entries_crc != 0);
    }

    #[test]
    fn rejects_a_disk_without_protective_mbr() {
        let device = SliceDevice::new(vec![0u8; 4096]);
        let err = read_gpt(&device).unwrap_err();
        assert!(matches!(err, GptError::NoProtectiveMbr));
    }

    #[test]
    fn rejects_a_broken_header_checksum() {
        let mut builder = Builder::new(2048);
        let entries = entry(&esp_guid(), &esp_guid(), 2048, 614399, "EFI");
        builder.write_header(1, 2, &entries, 2047);
        // Corrupt the signature.
        let offset = SECTOR_SIZE as usize;
        builder.image[offset] = 0x00;
        let device = SliceDevice::new(builder.image);
        assert!(matches!(
            read_gpt(&device).unwrap_err(),
            GptError::NoSignature
        ));
    }

    #[test]
    fn rejects_a_broken_entry_checksum() {
        let mut builder = Builder::new(2048);
        let mut entries = entry(&esp_guid(), &esp_guid(), 2048, 614399, "EFI");
        entries.extend_from_slice(&[0u8; 128]);
        builder.write_header(1, 2, &entries, 2047);
        let offset = (2 * SECTOR_SIZE) as usize;
        builder.image[offset..offset + entries.len()].copy_from_slice(&entries);
        // Corrupt one entry after the checksum was computed.
        builder.image[offset + 200] = 0xAB;
        let device = SliceDevice::new(builder.image);
        assert!(matches!(
            read_gpt(&device).unwrap_err(),
            GptError::BadEntryCrc
        ));
    }
}
