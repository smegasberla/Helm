//! Boot switching for Helm.
//!
//! This crate isolates every piece of platform specific behaviour needed to
//! talk to the UEFI firmware:
//!
//! * [`store::FirmwareStore`] abstracts the firmware variable store
//!   (`efivarfs` on Linux, `GetFirmwareEnvironmentVariableW` on Windows).
//! * [`switch::BootSwitcher`] implements the actual "boot the other OS on
//!   the next start" logic on top of that abstraction.
//! * [`devicepath`] encodes and decodes UEFI device paths.

pub mod devicepath;
pub mod reboot;
pub mod store;
pub mod switch;

#[cfg(unix)]
pub mod efivars;
#[cfg(windows)]
pub mod winenv;

pub use store::{FirmwareError, FirmwareStore, NullStore};
pub use switch::BootSwitcher;

/// Open the platform's firmware variable store.
///
/// Returns `None` when the machine was not booted through UEFI (legacy BIOS)
/// or when the firmware variable store is not accessible.
pub fn platform_store() -> Option<Box<dyn FirmwareStore>> {
    #[cfg(unix)]
    {
        match efivars::EfiVarStore::open() {
            Ok(store) => Some(Box::new(store)),
            Err(err) => {
                log::warn!("firmware variable store unavailable: {err}");
                None
            }
        }
    }
    #[cfg(windows)]
    {
        Some(Box::new(winenv::WindowsFirmwareStore::new()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// True when the machine is running in UEFI mode.
pub fn running_under_uefi() -> bool {
    #[cfg(unix)]
    {
        std::path::Path::new("/sys/firmware/efi").is_dir()
    }
    #[cfg(not(unix))]
    {
        true
    }
}
