//! Access to the UEFI variable store through the Win32 firmware APIs.
//!
//! `GetFirmwareEnvironmentVariableW` / `SetFirmwareEnvironmentVariableW`
//! operate on the firmware variable store of the running firmware. The
//! functions require the `SE_SYSTEM_ENVIRONMENT_NAME` privilege, which the
//! helper (running as a service) owns.
//!
//! There is no API to enumerate firmware variables on Windows, so the
//! `Boot####` namespace is probed instead, and `BootOrder` is used to restore
//! the firmware's own ordering.

use std::ffi::c_void;

use windows_sys::Win32::System::WindowsProgramming::{
    GetFirmwareEnvironmentVariableW, SetFirmwareEnvironmentVariableW,
};

use crate::devicepath::EFI_GLOBAL_VARIABLE_GUID;
use crate::store::{FirmwareError, FirmwareStore};

/// Highest `Boot####` index that is probed.
const MAX_BOOT_ENTRY: u32 = 0x0fff;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A firmware variable store backed by the Win32 firmware APIs.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsFirmwareStore;

impl WindowsFirmwareStore {
    pub fn new() -> Self {
        Self
    }
}

impl FirmwareStore for WindowsFirmwareStore {
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, FirmwareError> {
        let name_w = wide(name);
        let guid_w = wide(EFI_GLOBAL_VARIABLE_GUID);

        // First call with a zero sized buffer returns the required size.
        let size = unsafe {
            GetFirmwareEnvironmentVariableW(
                name_w.as_ptr(),
                guid_w.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if size == 0 {
            // The variable does not exist (or is not accessible).
            return Ok(None);
        }

        let mut buffer = vec![0u8; size as usize];
        let read = unsafe {
            GetFirmwareEnvironmentVariableW(
                name_w.as_ptr(),
                guid_w.as_ptr(),
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
            )
        };
        if read == 0 {
            return Err(FirmwareError::Unavailable(
                "GetFirmwareEnvironmentVariableW failed".into(),
            ));
        }
        buffer.truncate(read as usize);
        Ok(Some(buffer))
    }

    fn write(&self, name: &str, data: &[u8]) -> Result<(), FirmwareError> {
        let name_w = wide(name);
        let guid_w = wide(EFI_GLOBAL_VARIABLE_GUID);
        let ok = unsafe {
            SetFirmwareEnvironmentVariableW(
                name_w.as_ptr(),
                guid_w.as_ptr(),
                data.as_ptr() as *const c_void,
                data.len() as u32,
            )
        };
        if ok == 0 {
            return Err(FirmwareError::PermissionDenied);
        }
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<bool, FirmwareError> {
        // Setting a variable to an empty payload removes it.
        let name_w = wide(name);
        let guid_w = wide(EFI_GLOBAL_VARIABLE_GUID);
        let ok = unsafe {
            SetFirmwareEnvironmentVariableW(name_w.as_ptr(), guid_w.as_ptr(), std::ptr::null(), 0)
        };
        if ok == 0 {
            return Ok(false);
        }
        Ok(true)
    }

    fn list(&self) -> Result<Vec<String>, FirmwareError> {
        let mut names = Vec::new();
        for index in 0..=MAX_BOOT_ENTRY {
            let name = format!("Boot{index:04X}");
            if self.read(&name)?.is_some() {
                names.push(name);
            }
        }
        for name in ["BootOrder", "BootNext", "BootCurrent"] {
            if self.read(name)?.is_some() {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    fn is_uefi(&self) -> bool {
        true
    }
}
