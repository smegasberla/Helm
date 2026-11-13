//! Triggering a reboot of the machine.

use crate::store::FirmwareError;

/// Reboot the machine.
///
/// The helper runs with the privileges required for this; the GUI never calls
/// it directly.
pub fn reboot() -> Result<(), FirmwareError> {
    platform::reboot()
}

#[cfg(unix)]
mod platform {
    use crate::store::FirmwareError;

    pub fn reboot() -> Result<(), FirmwareError> {
        // `reboot(RB_AUTOBOOT)` needs CAP_SYS_BOOT, which the helper has.
        // SAFETY: the call either succeeds or returns an error; it never
        // touches invalid memory.
        let result = unsafe { libc::reboot(libc::RB_AUTOBOOT) };
        if result == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            // Fall back to the init system, which is friendlier in a desktop
            // session (it syncs the filesystems and lets applications close).
            let status = std::process::Command::new("systemctl")
                .arg("reboot")
                .status();
            match status {
                Ok(status) if status.success() => Ok(()),
                _ => Err(FirmwareError::Other(format!(
                    "reboot failed: {err} (systemctl: {status:?})"
                ))),
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::Shutdown::{
        ExitWindowsEx, InitiateSystemShutdownExW, EWX_REBOOT, REASON_MAJOR_APPLICATION,
        REASON_MINOR_OTHER,
    };

    use crate::store::FirmwareError;

    pub fn reboot() -> Result<(), FirmwareError> {
        let ok = unsafe { ExitWindowsEx(EWX_REBOOT, 0) };
        if ok != 0 {
            return Ok(());
        }
        // Fall back to the shutdown API, which also works from a service.
        let message: Vec<u16> = "Reboot requested by Helm\0".encode_utf16().collect();
        let ok = unsafe {
            InitiateSystemShutdownExW(
                std::ptr::null(),
                message.as_ptr(),
                0,
                1,
                1,
                REASON_MAJOR_APPLICATION | REASON_MINOR_OTHER,
            )
        };
        if ok != 0 {
            Ok(())
        } else {
            Err(FirmwareError::Other(
                "ExitWindowsEx and InitiateSystemShutdownExW both failed".into(),
            ))
        }
    }
}
