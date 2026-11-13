//! UEFI device path and `EFI_LOAD_OPTION` encoding.
//!
//! Helm never invents a boot entry from scratch when the firmware already
//! knows about one: the raw bytes of an existing `Boot####` variable are
//! copied into `BootNext`. The code in this module is therefore needed for
//!
//! * presenting the firmware's boot entries to the user (decoding), and
//! * building a synthetic entry when the firmware has no matching `Boot####`
//!   variable (encoding).
//!
//! # Firmware is not consistent
//!
//! The UEFI specification defines `EFI_LOAD_OPTION` as
//!
//! ```text
//! UINT32 Attributes; EFI_DEVICE_PATH FilePathList; CHAR16 Description[]; UINT8 OptionalData[];
//! ```
//!
//! Real firmware frequently stores the *description before* the device path
//! instead (AMI boards do this for both the "UEFI OS" and the "Windows Boot
//! Manager" entries). The decoder below therefore locates the device path by
//! walking the node chain instead of assuming a fixed offset, and accepts the
//! description from either side.

/// End of the whole device path list.
pub const END_ENTIRE_DEVICE_PATH: u8 = 0x7f;
/// Media device path node type (`MEDIA_DEVICE_PATH`).
pub const MEDIA_DEVICE_PATH: u8 = 0x04;
/// Hard drive media device path sub type.
pub const HARD_DRIVE_DEVICE_PATH: u8 = 0x01;
/// File path device path node type (also `MEDIA_DEVICE_PATH`).
pub const FILE_PATH_DEVICE_PATH: u8 = 0x04;
/// File path device path sub type.
pub const FILE_PATH_DEVICE_PATH_SUBTYPE: u8 = 0x04;
/// `MBR_TYPE_GPT` in `EFI_PARTITION_INFO`.
pub const MBR_TYPE_GPT: u8 = 0x02;
/// `SIGNATURE_TYPE_GUID` in `EFI_PARTITION_INFO`.
pub const SIGNATURE_TYPE_GUID: u8 = 0x02;

/// The `EFI_GLOBAL_VARIABLE` GUID, used by `BootOrder`, `BootNext`, ...
pub const EFI_GLOBAL_VARIABLE_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

/// Lowest node type that may start a device path.
const FIRST_NODE_TYPE: u8 = 0x01;
/// Highest node type that may start a device path (excluding the end node).
const LAST_NODE_TYPE: u8 = 0x05;

/// Parse a GUID in its canonical hyphenated form into the mixed endian byte
/// representation used by UEFI.
pub fn guid_from_str(input: &str) -> Option<[u8; 16]> {
    let hex: String = input.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut raw = [0u8; 16];
    for (i, byte) in raw.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    // UEFI stores the first three fields little endian, the last two as-is.
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
    Some(out)
}

/// Format a mixed endian UEFI GUID back into its canonical string form.
pub fn guid_to_string(guid: &[u8; 16]) -> String {
    let raw = [
        guid[3], guid[2], guid[1], guid[0], guid[5], guid[4], guid[7], guid[6], guid[8], guid[9],
        guid[10], guid[11], guid[12], guid[13], guid[14], guid[15],
    ];
    let mut s = String::with_capacity(36);
    for (i, byte) in raw.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// A single node of a device path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePathNode {
    pub node_type: u8,
    pub sub_type: u8,
    /// Payload of the node, without the 4 byte header.
    pub data: Vec<u8>,
}

impl DevicePathNode {
    /// True when the node terminates a device path list.
    pub fn is_end(&self) -> bool {
        self.node_type == END_ENTIRE_DEVICE_PATH
    }
}

/// Split a device path blob into its nodes. Malformed trailing data is
/// ignored instead of causing a hard failure: firmware images are known to be
/// sloppy.
pub fn parse_device_path(bytes: &[u8]) -> Vec<DevicePathNode> {
    let mut nodes = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= bytes.len() {
        let node_type = bytes[offset];
        let sub_type = bytes[offset + 1];
        let length = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        if length < 4 || offset + length > bytes.len() {
            break;
        }
        nodes.push(DevicePathNode {
            node_type,
            sub_type,
            data: bytes[offset + 4..offset + length].to_vec(),
        });
        offset += length;
        if node_type == END_ENTIRE_DEVICE_PATH {
            break;
        }
    }
    nodes
}

/// Try to walk a device path starting at `start`.
///
/// Returns the parsed nodes when the chain is well formed and terminated by
/// an end node.
fn walk_path(bytes: &[u8], start: usize) -> Option<Vec<DevicePathNode>> {
    let mut offset = start;
    let mut nodes = Vec::new();
    while offset + 4 <= bytes.len() {
        let node_type = bytes[offset];
        let sub_type = bytes[offset + 1];
        let length = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        if length < 4 || offset + length > bytes.len() {
            return None;
        }
        nodes.push(DevicePathNode {
            node_type,
            sub_type,
            data: bytes[offset + 4..offset + length].to_vec(),
        });
        offset += length;
        if node_type == END_ENTIRE_DEVICE_PATH {
            // A device path needs at least one real node before the end node.
            return if nodes.len() >= 2 { Some(nodes) } else { None };
        }
        if nodes.len() > 16 {
            return None;
        }
    }
    None
}

/// Locate the device path inside a raw `Boot####` variable.
///
/// Firmware disagrees on where the device path starts, so the blob is scanned
/// for a well formed node chain. The returned offset is the index of the first
/// node inside `bytes`.
pub fn find_device_path(bytes: &[u8]) -> Option<(usize, Vec<DevicePathNode>)> {
    let limit = bytes.len().min(512);
    for start in 0..limit {
        // A device path always starts with a hardware / acpi / messaging /
        // media / bios-boot node, never with the end node.
        let node_type = bytes[start];
        if !(FIRST_NODE_TYPE..=LAST_NODE_TYPE).contains(&node_type) {
            continue;
        }
        if let Some(nodes) = walk_path(bytes, start) {
            return Some((start, nodes));
        }
    }
    None
}

/// Build the `HD()` hard drive media device path node for a GPT partition.
///
/// `partition_sector_count` is expressed in 512 byte logical sectors, which is
/// what `efibootmgr` and the firmware itself use (the UEFI specification is
/// ambiguous here, but the sector interpretation is the one implemented by
/// every firmware Helm can be tested against).
pub fn build_media_node(
    partition_number: u32,
    partition_start_lba: u64,
    partition_sector_count: u64,
    partition_guid: &[u8; 16],
) -> Vec<u8> {
    let mut node = Vec::with_capacity(42);
    node.push(MEDIA_DEVICE_PATH);
    node.push(HARD_DRIVE_DEVICE_PATH);
    node.extend_from_slice(&42u16.to_le_bytes());
    node.extend_from_slice(&partition_number.to_le_bytes());
    node.extend_from_slice(&partition_start_lba.to_le_bytes());
    node.extend_from_slice(&partition_sector_count.to_le_bytes());
    node.extend_from_slice(partition_guid);
    node.push(MBR_TYPE_GPT);
    node.push(SIGNATURE_TYPE_GUID);
    node
}

/// Build a `File()` device path node for a loader path such as
/// `\EFI\grub\grubx64.efi`.
pub fn build_file_node(path: &str) -> Option<Vec<u8>> {
    if !path.starts_with('\\') {
        return None;
    }
    let mut utf16: Vec<u16> = path.encode_utf16().collect();
    utf16.push(0);

    let mut node = Vec::with_capacity(4 + utf16.len() * 2);
    node.push(FILE_PATH_DEVICE_PATH);
    node.push(FILE_PATH_DEVICE_PATH_SUBTYPE);
    node.extend_from_slice(&((4 + utf16.len() * 2) as u16).to_le_bytes());
    for unit in utf16 {
        node.extend_from_slice(&unit.to_le_bytes());
    }
    Some(node)
}

/// Build the end node that terminates a device path.
pub fn build_end_node() -> Vec<u8> {
    vec![END_ENTIRE_DEVICE_PATH, 0xff, 0x04, 0x00]
}

/// Extract the loader path (the `File()` node) from a device path.
pub fn loader_path_from_device_path(device_path: &[u8]) -> Option<String> {
    for node in parse_device_path(device_path) {
        if node.node_type == FILE_PATH_DEVICE_PATH && node.sub_type == FILE_PATH_DEVICE_PATH_SUBTYPE
        {
            let units: Vec<u16> = node
                .data
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
            let text = String::from_utf16_lossy(&units[..end]);
            return Some(text);
        }
    }
    None
}

/// Extract the partition GUID stored in the `HD()` node of a device path.
pub fn partition_guid_from_device_path(device_path: &[u8]) -> Option<String> {
    for node in parse_device_path(device_path) {
        if node.node_type == MEDIA_DEVICE_PATH
            && node.sub_type == HARD_DRIVE_DEVICE_PATH
            && node.data.len() >= 38
        {
            let guid: [u8; 16] = node.data[20..36].try_into().ok()?;
            return Some(guid_to_string(&guid));
        }
    }
    None
}

/// A decoded `EFI_LOAD_OPTION`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoadOption {
    /// Device path pointing at the loader.
    pub device_path: Vec<u8>,
    /// Human readable description (UCS-2, without the terminating NUL).
    pub description: String,
    /// Vendor specific trailing data.
    pub optional_data: Vec<u8>,
    /// The `Attributes` field, when the blob starts with one.
    pub attributes: Option<u32>,
}

/// Decode a NUL terminated UCS-2 string at the beginning of `bytes`.
///
/// Returns the string and the number of bytes it occupies.
fn read_utf16_string(bytes: &[u8]) -> Option<(String, usize)> {
    if bytes.len() < 2 {
        return None;
    }
    let mut units: Vec<u16> = Vec::new();
    let mut offset = 0usize;
    loop {
        if offset + 2 > bytes.len() {
            return None;
        }
        let unit = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        offset += 2;
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    if units.is_empty() {
        return Some((String::new(), offset));
    }
    // Reject control characters: this filters out binary blobs that would
    // otherwise be decoded into garbage.
    if units
        .iter()
        .any(|u| (*u < 0x20 && *u != 0x09) || *u == 0x7f)
    {
        return None;
    }
    Some((String::from_utf16_lossy(&units), offset))
}

/// Decode a `LOAD_OPTION` blob as stored in a `Boot####` variable.
pub fn parse_load_option(bytes: &[u8]) -> LoadOption {
    let mut result = LoadOption::default();
    let Some((dp_start, nodes)) = find_device_path(bytes) else {
        return result;
    };

    let dp_end = dp_start + nodes.iter().map(|n| n.data.len() + 4).sum::<usize>();
    result.device_path = bytes[dp_start..dp_end.min(bytes.len())].to_vec();

    // Everything in front of the device path: an optional 4 byte attributes
    // field followed by an optional description.
    let prefix = &bytes[..dp_start];
    if prefix.len() >= 4 {
        result.attributes = Some(u32::from_le_bytes([
            prefix[0], prefix[1], prefix[2], prefix[3],
        ]));
        if let Some((text, _)) = read_utf16_string(&prefix[4..]) {
            if !text.is_empty() {
                result.description = text;
            }
        }
    }

    // Everything behind the device path: the description (in the layout used
    // by the UEFI specification) followed by the optional data.
    let suffix = &bytes[dp_end.min(bytes.len())..];
    if let Some((text, used)) = read_utf16_string(suffix) {
        if !text.is_empty() && result.description.is_empty() {
            result.description = text;
        }
        result.optional_data = suffix[used.min(suffix.len())..].to_vec();
    } else {
        result.optional_data = suffix.to_vec();
    }

    result
}

/// Encode a `LOAD_OPTION` blob from a device path and a description.
pub fn build_load_option(device_path: &[u8], description: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(device_path.len() + description.len() * 2 + 2);
    out.extend_from_slice(device_path);
    for unit in description.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_roundtrip() {
        let guid = guid_from_str(EFI_GLOBAL_VARIABLE_GUID).unwrap();
        assert_eq!(
            guid,
            [
                0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03,
                0x2b, 0x8c
            ]
        );
        assert_eq!(guid_to_string(&guid), EFI_GLOBAL_VARIABLE_GUID);
    }

    /// A GUID captured from the device path of a real firmware boot entry.
    #[test]
    fn guid_matches_firmware_bytes() {
        let guid = guid_from_str("935b3754-3e31-4687-a1f8-e69ae456a9e5").unwrap();
        assert_eq!(
            guid,
            [
                0x54, 0x37, 0x5b, 0x93, 0x31, 0x3e, 0x87, 0x46, 0xa1, 0xf8, 0xe6, 0x9a, 0xe4, 0x56,
                0xa9, 0xe5
            ]
        );
        assert_eq!(
            guid_to_string(&guid),
            "935b3754-3e31-4687-a1f8-e69ae456a9e5"
        );
    }

    #[test]
    fn guid_rejects_garbage() {
        assert!(guid_from_str("not-a-guid").is_none());
        assert!(guid_from_str("").is_none());
    }

    #[test]
    fn media_node_layout() {
        let guid = guid_from_str("28732ac1-8ffa-4d0f-9e51-9ec6e0b2b3c4").unwrap();
        let node = build_media_node(2, 2048, 614400, &guid);
        assert_eq!(node.len(), 42);
        assert_eq!(node[0], MEDIA_DEVICE_PATH);
        assert_eq!(node[1], HARD_DRIVE_DEVICE_PATH);
        assert_eq!(u16::from_le_bytes([node[2], node[3]]) as usize, 42);
        assert_eq!(u32::from_le_bytes([node[4], node[5], node[6], node[7]]), 2);
        assert_eq!(
            u64::from_le_bytes([
                node[8], node[9], node[10], node[11], node[12], node[13], node[14], node[15]
            ]),
            2048
        );
        assert_eq!(
            u64::from_le_bytes([
                node[16], node[17], node[18], node[19], node[20], node[21], node[22], node[23]
            ]),
            614400
        );
        assert_eq!(node[40], MBR_TYPE_GPT);
        assert_eq!(node[41], SIGNATURE_TYPE_GUID);
    }

    #[test]
    fn file_node_roundtrip() {
        let node = build_file_node("\\EFI\\grub\\grubx64.efi").unwrap();
        assert_eq!(node[0], FILE_PATH_DEVICE_PATH);
        assert_eq!(node[1], FILE_PATH_DEVICE_PATH_SUBTYPE);
        let len = u16::from_le_bytes([node[2], node[3]]) as usize;
        assert_eq!(len, node.len());
        assert_eq!(len % 2, 0);
        let path = loader_path_from_device_path(&node).unwrap();
        assert_eq!(path, "\\EFI\\grub\\grubx64.efi");
    }

    #[test]
    fn file_node_requires_absolute_path() {
        assert!(build_file_node("EFI/grub").is_none());
    }

    #[test]
    fn load_option_roundtrip() {
        let media = build_media_node(
            1,
            2048,
            1024 * 1024,
            &guid_from_str("28732ac1-8ffa-4d0f-9e51-9ec6e0b2b3c4").unwrap(),
        );
        let file = build_file_node("\\EFI\\Microsoft\\Boot\\bootmgfw.efi").unwrap();
        let mut dp = media;
        dp.extend_from_slice(&file);
        dp.extend_from_slice(&build_end_node());

        let option = build_load_option(&dp, "Windows Boot Manager");
        let parsed = parse_load_option(&option);
        assert_eq!(parsed.description, "Windows Boot Manager");
        assert_eq!(
            loader_path_from_device_path(&parsed.device_path).unwrap(),
            "\\EFI\\Microsoft\\Boot\\bootmgfw.efi"
        );
    }

    #[test]
    fn parse_load_option_tolerates_garbage() {
        let parsed = parse_load_option(&[0xff, 0x01, 0x02]);
        assert_eq!(parsed.description, "");
        assert!(parsed.device_path.is_empty());
    }

    /// Layout used by AMI firmware: attributes, description, device path,
    /// empty description, optional data.
    #[test]
    fn parses_description_before_device_path() {
        let media = build_media_node(
            1,
            2048,
            614400,
            &guid_from_str("54375b93-313e-8746-a1f8-e69ae456a9e5").unwrap(),
        );
        let file = build_file_node("\\EFI\\BOOT\\BOOTX64.EFI").unwrap();
        let mut dp = media;
        dp.extend_from_slice(&file);
        dp.extend_from_slice(&build_end_node());

        let mut blob = Vec::new();
        blob.extend_from_slice(&1u32.to_le_bytes());
        for unit in "UEFI OS".encode_utf16() {
            blob.extend_from_slice(&unit.to_le_bytes());
        }
        blob.extend_from_slice(&0u16.to_le_bytes());
        blob.extend_from_slice(&dp);
        blob.extend_from_slice(&0u16.to_le_bytes());
        blob.extend_from_slice(b"BO");

        let parsed = parse_load_option(&blob);
        assert_eq!(parsed.description, "UEFI OS");
        assert_eq!(parsed.attributes, Some(1));
        assert_eq!(
            loader_path_from_device_path(&parsed.device_path).unwrap(),
            "\\EFI\\BOOT\\BOOTX64.EFI"
        );
        assert_eq!(parsed.optional_data, b"BO".to_vec());
    }

    /// Layout defined by the UEFI specification: attributes, device path,
    /// description, optional data.
    #[test]
    fn parses_description_after_device_path() {
        let media = build_media_node(
            1,
            2048,
            614400,
            &guid_from_str("dbda3994-f99b-4b44-89a6-e08db5daa1a2").unwrap(),
        );
        let file = build_file_node("\\EFI\\Microsoft\\Boot\\bootmgfw.efi").unwrap();
        let mut dp = media;
        dp.extend_from_slice(&file);
        dp.extend_from_slice(&build_end_node());

        let mut blob = Vec::new();
        blob.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(&dp);
        for unit in "Windows Boot Manager".encode_utf16() {
            blob.extend_from_slice(&unit.to_le_bytes());
        }
        blob.extend_from_slice(&0u16.to_le_bytes());
        blob.extend_from_slice(&[0xaa, 0xbb]);

        let parsed = parse_load_option(&blob);
        assert_eq!(parsed.description, "Windows Boot Manager");
        assert_eq!(
            loader_path_from_device_path(&parsed.device_path).unwrap(),
            "\\EFI\\Microsoft\\Boot\\bootmgfw.efi"
        );
    }

    /// The real bytes captured from a UEFI firmware.
    #[test]
    fn parses_real_firmware_blob() {
        // EFI_LOAD_OPTION captured from /sys/firmware/efi/efivars/Boot0001 of
        // an AMI board. The description carries a '^' marker that the
        // firmware uses for its own entries.
        let blob: Vec<u8> = vec![
            0x01, 0x00, 0x00, 0x00, // attributes
            0x5e, 0x00, 0x55, 0x00, 0x45, 0x00, 0x46, 0x00, 0x49, 0x00, 0x20, 0x00, 0x4f, 0x00,
            0x53, 0x00, 0x00, 0x00, // "^UEFI OS"
            0x04, 0x01, 0x2a, 0x00, // HD() node
            0x01, 0x00, 0x00, 0x00, // partition number 1
            0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // start LBA 2048
            0x00, 0x60, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, // 614400 sectors == 300 MiB
            0x54, 0x37, 0x5b, 0x93, 0x31, 0x3e, 0x87, 0x46, 0xa1, 0xf8, 0xe6, 0x9a, 0xe4, 0x56,
            0xa9, 0xe5, // partition GUID
            0x02, 0x02, // MBR type GPT, signature type GUID
            0x04, 0x04, 0x30, 0x00, // File() node, length 48
            0x5c, 0x00, 0x45, 0x00, 0x46, 0x00, 0x49, 0x00, 0x5c, 0x00, 0x42, 0x00, 0x4f, 0x00,
            0x4f, 0x00, 0x54, 0x00, 0x5c, 0x00, 0x42, 0x00, 0x4f, 0x00, 0x4f, 0x00, 0x54, 0x00,
            0x58, 0x00, 0x36, 0x00, 0x34, 0x00, 0x2e, 0x00, 0x45, 0x00, 0x46, 0x00, 0x49, 0x00,
            0x00, 0x00, // "\EFI\BOOT\BOOTX64.EFI"
            0x7f, 0xff, 0x04, 0x00, // end node
            0x00, 0x00, // empty description
            0x42, 0x4f, // optional data
        ];
        let parsed = parse_load_option(&blob);
        assert_eq!(parsed.description, "^UEFI OS");
        assert_eq!(parsed.attributes, Some(1));
        assert_eq!(
            loader_path_from_device_path(&parsed.device_path).unwrap(),
            "\\EFI\\BOOT\\BOOTX64.EFI"
        );
        // The canonical form matches the PARTUUID reported by the kernel for
        // the ESP this entry points at.
        assert_eq!(
            partition_guid_from_device_path(&parsed.device_path).unwrap(),
            "935b3754-3e31-4687-a1f8-e69ae456a9e5"
        );
        assert_eq!(parsed.optional_data, b"BO".to_vec());
    }
}
