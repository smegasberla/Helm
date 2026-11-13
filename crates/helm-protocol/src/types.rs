//! Data types shared between the privileged helper and the unprivileged GUI.
//!
//! Everything in this crate is intentionally platform-neutral so that the GUI
//! can be compiled for Linux and Windows from a single source of truth.

use serde::{Deserialize, Serialize};

/// Maximum size of a single IPC frame (8 MiB). Guards against a malicious or
/// buggy peer from making the helper allocate unbounded memory.
pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

/// Path of the helper's local socket.
///
/// * Linux: a Unix domain socket inside the systemd runtime directory.
/// * Windows: a named pipe in the `\\.\pipe` namespace.
pub const HELPER_SOCKET: &str = if cfg!(windows) {
    r"\\.\pipe\helm-helper"
} else {
    "/run/helm/helper.sock"
};

/// The path of the socket of the helper.
///
/// The environment variable `HELM_HELPER_SOCKET` overrides the default,
/// which is handy while developing.
pub fn helper_socket_path() -> String {
    std::env::var("HELM_HELPER_SOCKET").unwrap_or_else(|_| HELPER_SOCKET.to_string())
}

/// Filesystem flavour detected on a partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSystemKind {
    Ntfs,
    Ext2,
    Ext3,
    Ext4,
    Btrfs,
    Xfs,
    Fat12,
    Fat16,
    Fat32,
    Unknown,
}

impl FileSystemKind {
    pub fn is_readable(&self) -> bool {
        !matches!(self, FileSystemKind::Unknown)
    }
}

/// A single GPT partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionInfo {
    /// 1-based partition number as reported by the firmware.
    pub number: u32,
    /// GPT partition type GUID, lower-case hyphenated.
    pub type_guid: String,
    /// GPT unique partition GUID, lower-case hyphenated.
    pub unique_guid: String,
    /// Human readable partition label (GPT attribute bit 2).
    pub label: String,
    /// First LBA of the partition.
    pub start_lba: u64,
    /// Last LBA of the partition (inclusive).
    pub end_lba: u64,
    pub size_bytes: u64,
    /// True when the type GUID is `EFI System Partition`.
    pub is_esp: bool,
    /// True when the type GUID is `Microsoft Basic Data` / `Microsoft Reserved`.
    pub is_windows_data: bool,
    pub fs: FileSystemKind,
}

impl PartitionInfo {
    /// Number of 512-byte logical sectors in the partition.
    pub fn sector_count(&self) -> u64 {
        self.end_lba.saturating_sub(self.start_lba) + 1
    }
}

/// A physical disk visible to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskInfo {
    /// Stable index inside the detection report.
    pub index: u32,
    /// Platform path of the raw block device (`/dev/nvme0n1`, `\\.\PhysicalDrive0`).
    pub path: String,
    pub model: String,
    pub serial: Option<String>,
    pub size_bytes: u64,
    /// Sector size used for all LBA maths (usually 512).
    pub sector_size: u32,
    pub removable: bool,
    pub gpt: bool,
    pub partitions: Vec<PartitionInfo>,
}

/// The kind of operating system found on a partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsKind {
    Windows,
    Linux,
    MacOs,
}

/// How the firmware should be pointed at the selected OS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BootTargetMode {
    /// Re-use an existing `Boot####` NVRAM entry verbatim. This is the safest
    /// path: the exact `LOAD_OPTION` blob the firmware already knows about is
    /// copied into `BootNext`.
    ExistingEntry {
        /// NVRAM variable name, e.g. `Boot0003`.
        source_var: String,
        /// Hex encoded raw `LOAD_OPTION` payload of the source variable.
        load_option_hex: String,
    },
    /// Build a `LOAD_OPTION` from scratch: a `HD()` node for the ESP that
    /// holds the loader plus a `File()` node for the loader path.
    Synthetic {
        /// `\EFI\grub\grubx64.efi` style loader path inside the ESP.
        loader_path: String,
        /// Index of the partition (1-based) holding the loader.
        esp_partition_number: u32,
        /// Unique partition GUID of the ESP, used for the `HD()` node signature.
        esp_unique_guid: String,
        /// Start LBA of the ESP, used for the `HD()` node.
        esp_start_lba: u64,
        /// Size of the ESP in 512 byte sectors, used for the `HD()` node.
        esp_sector_count: u64,
    },
}

/// Everything needed to make the firmware boot a given OS on the next boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootTarget {
    /// Human readable description, e.g. `EFI\grub\grubx64.efi`.
    pub label: String,
    pub mode: BootTargetMode,
}

/// An operating system discovered on one of the disks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedOs {
    /// Stable identifier inside a detection report, used by the GUI to refer
    /// back to this entry when the user asks for a switch.
    pub id: String,
    /// Display name, e.g. `Ubuntu 24.04` or `Windows 11 Pro`.
    pub name: String,
    pub kind: OsKind,
    /// Index into [`DetectionReport::disks`].
    pub disk_index: u32,
    /// Human readable disk label, e.g. `Samsung SSD 990 PRO 2TB`.
    pub disk_label: String,
    /// 1-based partition number on that disk.
    pub partition_number: u32,
    pub partition_label: String,
    pub fs: FileSystemKind,
    /// Distro / edition string read from `/etc/os-release` or the Windows
    /// registry hive.
    pub distro: Option<String>,
    /// Icon hint for the GUI (`linux`, `windows`, `apple`).
    pub icon: Option<String>,
    /// Boot loader that will be pointed at. `None` when the OS was detected
    /// but no usable UEFI loader was found (e.g. legacy-only install).
    pub boot: Option<BootTarget>,
    /// Human readable list of the signals that produced this detection.
    pub evidence: Vec<String>,
    /// True when the OS can actually be booted through UEFI.
    pub bootable: bool,
}

/// An entry found in the firmware's boot variable store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareBootEntry {
    /// NVRAM variable name, e.g. `Boot0002`.
    pub var_name: String,
    /// Description string stored in the `LOAD_OPTION` blob.
    pub description: String,
    /// Optional device path (hex) the entry points at.
    pub device_path_hex: Option<String>,
    /// Loader path resolved from the device path, e.g. `\EFI\grub\grubx64.efi`.
    pub loader_path: Option<String>,
}

/// Full result of a detection run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionReport {
    pub disks: Vec<DiskInfo>,
    pub oses: Vec<DetectedOs>,
    pub boot_entries: Vec<FirmwareBootEntry>,
    /// Set when the machine is not booted through UEFI.
    pub uefi: bool,
    /// Human readable notes about degraded capabilities (e.g. no efivars access).
    pub warnings: Vec<String>,
}

/// A request sent from the GUI to the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness / capability probe.
    Ping,
    /// Ask the helper to re-run disk and OS detection.
    Detect,
    /// Set `BootNext` to the given OS (identified by [`DetectedOs::id`]).
    SetBootNext { os_id: String },
    /// Reboot the machine.
    Reboot,
    /// Return the currently pending `BootNext` value, if any.
    GetBootNext,
}

/// A response sent from the helper back to the GUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        /// Free-form payload, serialised per-operation.
        #[serde(default)]
        data: serde_json::Value,
    },
    Error {
        message: String,
        /// Machine readable reason, e.g. `not_authorized`.
        reason: Option<String>,
    },
}

impl Response {
    pub fn ok(value: serde_json::Value) -> Self {
        Response::Ok { data: value }
    }

    pub fn error(message: impl Into<String>, reason: Option<&str>) -> Self {
        Response::Error {
            message: message.into(),
            reason: reason.map(|r| r.to_string()),
        }
    }
}

/// Capability flags advertised by the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperCapabilities {
    pub version: String,
    /// True when the helper can write firmware variables.
    pub can_write_boot_variables: bool,
    /// True when the helper can trigger a reboot.
    pub can_reboot: bool,
    /// True when detection runs inside a UEFI boot.
    pub uefi: bool,
}

/// Helper metadata returned by `Ping`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperInfo {
    pub capabilities: HelperCapabilities,
    /// Unix user id of the connected peer, when the platform can report it.
    pub peer_uid: Option<u32>,
}
