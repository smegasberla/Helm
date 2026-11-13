//! Access to the UEFI firmware variable store.
//!
//! The store is abstracted behind [`FirmwareStore`] so that the rest of the
//! application never has to care whether it runs on Linux (efivarfs) or on
//! Windows (`GetFirmwareEnvironmentVariable`).

use helm_protocol::FirmwareBootEntry;

use crate::devicepath;

/// Attributes used for the variables Helm creates: non-volatile, available to
/// the boot services and to the runtime.
pub const VAR_NON_VOLATILE: u32 = 0x0000_0001;
pub const VAR_BOOT_SERVICE: u32 = 0x0000_0002;
pub const VAR_RUNTIME: u32 = 0x0000_0004;
pub const VAR_ATTRIBUTES: u32 = VAR_NON_VOLATILE | VAR_BOOT_SERVICE | VAR_RUNTIME;

#[derive(Debug, thiserror::Error)]
pub enum FirmwareError {
    #[error("the firmware variable store is not available ({0})")]
    Unavailable(String),
    #[error("permission denied while accessing the firmware variable store")]
    PermissionDenied,
    #[error("firmware variable {0} not found")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("firmware error: {0}")]
    Other(String),
}

/// Read/write access to the UEFI variable store.
pub trait FirmwareStore: Send + Sync {
    /// Read the raw payload of a variable.
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, FirmwareError>;

    /// Create or replace a variable.
    fn write(&self, name: &str, data: &[u8]) -> Result<(), FirmwareError>;

    /// Remove a variable. Returns `true` when something was removed.
    fn delete(&self, name: &str) -> Result<bool, FirmwareError>;

    /// List the names of the variables that are visible to the caller.
    fn list(&self) -> Result<Vec<String>, FirmwareError>;

    /// True when the machine booted through UEFI.
    fn is_uefi(&self) -> bool;
}

/// Names of the `Boot####` variables present in the store.
pub fn boot_variable_names(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = names
        .iter()
        .filter(|n| {
            n.len() == 8 && n.starts_with("Boot") && n[4..].chars().all(|c| c.is_ascii_hexdigit())
        })
        .cloned()
        .collect();
    out.sort();
    out
}

/// Clean up a boot entry description for display.
///
/// Some firmware prefixes its own entries with a marker: the AMI firmware
/// uses `^` and `t` for the entries it manages itself.
pub fn clean_description(text: &str) -> String {
    let trimmed = text.trim();
    let stripped = trimmed.strip_prefix('^').unwrap_or(trimmed).trim();
    // A single lower case letter followed by an upper case one is a marker
    // as well ("tWindows Boot Manager").
    let bytes = stripped.as_bytes();
    let without_marker = if bytes.len() > 3
        && bytes[0].is_ascii_lowercase()
        && bytes[1].is_ascii_uppercase()
        && (stripped.starts_with('t') || stripped.starts_with('T'))
    {
        stripped[1..].trim()
    } else {
        stripped
    };
    if without_marker.is_empty() {
        trimmed.to_string()
    } else {
        without_marker.to_string()
    }
}

/// Decode every `Boot####` variable into a human readable entry.
pub fn decode_boot_entries(
    store: &dyn FirmwareStore,
) -> Result<Vec<FirmwareBootEntry>, FirmwareError> {
    let names = store.list()?;
    let mut entries = Vec::new();
    for name in boot_variable_names(&names) {
        let Some(data) = store.read(&name)? else {
            continue;
        };
        let option = devicepath::parse_load_option(&data);
        entries.push(FirmwareBootEntry {
            var_name: name.clone(),
            description: {
                let cleaned = clean_description(&option.description);
                if cleaned.is_empty() {
                    name.clone()
                } else {
                    cleaned
                }
            },
            device_path_hex: Some(hex_encode(&option.device_path)),
            loader_path: devicepath::loader_path_from_device_path(&option.device_path),
        });
    }
    Ok(entries)
}

/// Hex encode a byte slice (lower case, no separators).
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decode a hex string produced by [`hex_encode`].
pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

impl<T: FirmwareStore + ?Sized> FirmwareStore for Box<T> {
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, FirmwareError> {
        (**self).read(name)
    }

    fn write(&self, name: &str, data: &[u8]) -> Result<(), FirmwareError> {
        (**self).write(name, data)
    }

    fn delete(&self, name: &str) -> Result<bool, FirmwareError> {
        (**self).delete(name)
    }

    fn list(&self) -> Result<Vec<String>, FirmwareError> {
        (**self).list()
    }

    fn is_uefi(&self) -> bool {
        (**self).is_uefi()
    }
}

/// A store that never contains anything. Used as a fallback when the platform
/// has no firmware variable store (legacy BIOS).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullStore;

impl FirmwareStore for NullStore {
    fn read(&self, _name: &str) -> Result<Option<Vec<u8>>, FirmwareError> {
        Ok(None)
    }

    fn write(&self, _name: &str, _data: &[u8]) -> Result<(), FirmwareError> {
        Err(FirmwareError::Unavailable(
            "the firmware variable store is not available".into(),
        ))
    }

    fn delete(&self, _name: &str) -> Result<bool, FirmwareError> {
        Ok(false)
    }

    fn list(&self) -> Result<Vec<String>, FirmwareError> {
        Ok(Vec::new())
    }

    fn is_uefi(&self) -> bool {
        false
    }
}

/// Human readable description of a firmware error.
pub fn describe(error: &FirmwareError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_variable_filtering() {
        let names = vec![
            "Boot0000".to_string(),
            "Boot000f".to_string(),
            "BootOrder".to_string(),
            "Boot".to_string(),
            "boot0001".to_string(),
        ];
        let filtered = boot_variable_names(&names);
        assert_eq!(
            filtered,
            vec!["Boot0000".to_string(), "Boot000f".to_string()]
        );
    }

    #[test]
    fn description_cleanup() {
        assert_eq!(clean_description("^UEFI OS"), "UEFI OS");
        assert_eq!(
            clean_description("tWindows Boot Manager"),
            "Windows Boot Manager"
        );
        assert_eq!(clean_description("ubuntu"), "ubuntu");
        assert_eq!(clean_description("Intel"), "Intel");
        assert_eq!(
            clean_description("  Windows Boot Manager "),
            "Windows Boot Manager"
        );
        assert_eq!(clean_description(""), "");
    }

    #[test]
    fn hex_roundtrip() {
        let data = vec![0u8, 1, 15, 16, 255];
        let text = hex_encode(&data);
        assert_eq!(text, "00010f10ff");
        assert_eq!(hex_decode(&text).unwrap(), data);
        assert!(hex_decode("abc").is_none());
        assert!(hex_decode("zz").is_none());
    }
}
