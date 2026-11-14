//! Handing a PCI function over to `vfio-pci` and taking it back.
//!
//! The host driver has to let go of the card before QEMU can own it, and the
//! card has to sit alone in its IOMMU group: a group is the smallest unit the
//! IOMMU can isolate, so a function that shares one with another device cannot
//! be passed through without taking the other device with it.
//!
//! Every operation here writes to the sysfs, which is why the module is only
//! ever called from the privileged helper.

use std::path::Path;

/// The driver that owns a passed-through function.
pub const VFIO_DRIVER: &str = "vfio-pci";

/// The driver the card goes back to once the guest is gone.
///
/// The profile records the driver it actually took the card from, so this is
/// only the fallback for a card nothing owned at plan time.
pub const HOST_DRIVER: &str = "nvidia";

/// What can go wrong while taking a card away from the host.
#[derive(Debug, thiserror::Error)]
pub enum VfioError {
    #[error("the PCI function {0} does not exist on this machine")]
    Missing(String),
    #[error("the IOMMU is not enabled: add intel_iommu=on (or amd_iommu=on) to the kernel command line")]
    NoIommu,
    #[error("the PCI function {0} has no IOMMU group, so it cannot be isolated")]
    NoGroup(String),
    #[error(
        "the PCI function {0} shares IOMMU group {1} with {2} other function(s); they would have \
         to be passed through together"
    )]
    SharedGroup(String, String, usize),
    #[error("{0}")]
    Io(String),
}

/// The identity and the isolation of one PCI function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciFunction {
    /// `0000:01:00.0` style address.
    pub address: String,
    /// `10de:2520` style vendor and device id.
    pub vendor_device: String,
    /// The IOMMU group the function belongs to, when the IOMMU is on.
    pub iommu_group: Option<String>,
    /// The other functions that share the group with this one.
    pub group_peers: Vec<String>,
    /// The driver that owns the function right now.
    pub driver: Option<String>,
}

impl PciFunction {
    /// True when the card can be handed to a guest on its own.
    pub fn is_isolated(&self) -> bool {
        self.iommu_group.is_some() && self.group_peers.is_empty()
    }

    /// True when the function already belongs to `vfio-pci`.
    pub fn is_bound_to_vfio(&self) -> bool {
        self.driver.as_deref() == Some(VFIO_DRIVER)
    }
}

/// Describe one PCI function.
pub fn describe(address: &str) -> Result<PciFunction, VfioError> {
    let device = format!("/sys/bus/pci/devices/{address}");
    if !Path::new(&device).is_dir() {
        return Err(VfioError::Missing(address.to_string()));
    }

    let driver = std::fs::read_link(format!("{device}/driver"))
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        });

    let iommu_group = std::fs::read_link(format!("{device}/iommu_group"))
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        });

    let mut group_peers = Vec::new();
    if let Some(group) = &iommu_group {
        let members = format!("/sys/kernel/iommu_groups/{group}/devices");
        let mut entries: Vec<String> = std::fs::read_dir(members)
            .map(|dir| {
                dir.filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        group_peers = entries
            .into_iter()
            .filter(|member| member != address)
            .collect();
    }

    Ok(PciFunction {
        address: address.to_string(),
        vendor_device: vendor_and_device(address).unwrap_or_default(),
        iommu_group,
        group_peers,
        driver,
    })
}

/// True when the kernel was started with an active IOMMU.
pub fn is_iommu_enabled() -> bool {
    Path::new("/sys/kernel/iommu_groups").is_dir()
        && std::fs::read_dir("/sys/kernel/iommu_groups")
            .map(|dir| dir.filter_map(|entry| entry.ok()).next().is_some())
            .unwrap_or(false)
}

/// The kernel driver of a function, as `driver/override` sees it.
pub fn current_driver(address: &str) -> Option<String> {
    std::fs::read_link(format!("/sys/bus/pci/devices/{address}/driver"))
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
}

/// Take the function away from `vfio-pci` and give it back to `driver`.
///
/// This runs after the guest is gone: the host needs its card back, otherwise
/// the NVIDIA driver, `prime-run` and every CUDA program stay broken until the
/// next reboot.
pub fn release(address: &str, driver: &str) -> Result<(), VfioError> {
    if current_driver(address).as_deref() == Some(driver) {
        return Ok(());
    }
    if current_driver(address).as_deref() == Some(VFIO_DRIVER) {
        write(&format!("/sys/bus/pci/drivers/{VFIO_DRIVER}/unbind"), address)?;
    }
    // The override has to be dropped before the original driver can claim the
    // device again; leaving it behind is what makes a card vanish after one run.
    let _ = write(
        &format!("/sys/bus/pci/devices/{address}/driver_override"),
        "\n",
    );
    write(&format!("/sys/bus/pci/drivers/{driver}/bind"), address)
}

/// Give the function to `vfio-pci`.
pub fn bind(address: &str) -> Result<(), VfioError> {
    let function = describe(address)?;
    if !is_iommu_enabled() {
        return Err(VfioError::NoIommu);
    }
    if function.iommu_group.is_none() {
        return Err(VfioError::NoGroup(address.to_string()));
    }
    if !function.group_peers.is_empty() {
        return Err(VfioError::SharedGroup(
            address.to_string(),
            function.iommu_group.clone().unwrap_or_default(),
            function.group_peers.len(),
        ));
    }
    if function.is_bound_to_vfio() {
        return Ok(());
    }

    // vfio-pci may not be loaded yet: without its driver directory there is
    // nothing to bind the address to.
    if !Path::new(&format!("/sys/bus/pci/drivers/{VFIO_DRIVER}")).is_dir() {
        let loaded = std::process::Command::new("modprobe")
            .arg(VFIO_DRIVER)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !loaded {
            return Err(VfioError::Io(format!(
                "{VFIO_DRIVER} is not available: install the vfio modules of this kernel"
            )));
        }
    }

    if let Some(driver) = function.driver {
        write(&format!("/sys/bus/pci/drivers/{driver}/unbind"), address)?;
    }
    write(
        &format!("/sys/bus/pci/devices/{address}/driver_override"),
        VFIO_DRIVER,
    )?;
    write(&format!("/sys/bus/pci/drivers/{VFIO_DRIVER}/bind"), address)
}

/// The `vendor:device` pair of a function, used in the log and in the UI.
fn vendor_and_device(address: &str) -> Option<String> {
    let vendor = std::fs::read_to_string(format!("/sys/bus/pci/devices/{address}/vendor")).ok()?;
    let device = std::fs::read_to_string(format!("/sys/bus/pci/devices/{address}/device")).ok()?;
    Some(format!(
        "{}:{}",
        vendor.trim().trim_start_matches("0x"),
        device.trim().trim_start_matches("0x")
    ))
}

/// Write one value into a sysfs attribute.
fn write(path: &str, value: &str) -> Result<(), VfioError> {
    std::fs::write(path, value).map_err(|err| {
        VfioError::Io(format!(
            "{path} could not be written ({err}); this needs root rights"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_function_alone_in_its_group_is_isolated() {
        let function = PciFunction {
            address: "0000:01:00.0".into(),
            vendor_device: "10de:2520".into(),
            iommu_group: Some("15".into()),
            group_peers: Vec::new(),
            driver: Some("nvidia".into()),
        };
        assert!(function.is_isolated());
        assert!(!function.is_bound_to_vfio());
    }

    #[test]
    fn a_shared_group_is_not_isolated() {
        let function = PciFunction {
            address: "0000:00:01.0".into(),
            vendor_device: "8086:4601".into(),
            iommu_group: Some("3".into()),
            group_peers: vec!["0000:00:01.1".into()],
            driver: None,
        };
        assert!(!function.is_isolated());
    }

    #[test]
    fn a_function_without_a_group_is_not_isolated() {
        let function = PciFunction {
            address: "0000:02:00.0".into(),
            vendor_device: "1234:5678".into(),
            iommu_group: None,
            group_peers: Vec::new(),
            driver: None,
        };
        assert!(!function.is_isolated());
    }

    #[test]
    fn the_driver_of_the_card_in_this_machine_is_read() {
        // The check must not fail on a machine that simply has no such card.
        match describe("0000:01:00.0") {
            Ok(function) => assert_eq!(function.address, "0000:01:00.0"),
            Err(VfioError::Missing(_)) => {}
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}
