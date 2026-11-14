//! Turning a profile into the command line of a QEMU process.
//!
//! The arguments are built as data and only then handed to the process, so the
//! whole launch plan can be printed, reviewed and tested without ever starting
//! a virtual machine.

use crate::profile::{Display, Profile, OVMF_CODE, OVMF_VARS};
use crate::vfio;

/// The binary the profile runs.
pub const QEMU_BINARY: &str = "qemu-system-x86_64";

/// A complete launch plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    /// Render the plan the way it would be typed in a shell.
    pub fn render(&self) -> String {
        let mut text = self.program.clone();
        for arg in &self.args {
            text.push(' ');
            if arg.contains(' ') || arg.contains('\'') {
                text.push('\'');
                text.push_str(&arg.replace('\'', "'\\''"));
                text.push('\'');
            } else {
                text.push_str(arg);
            }
        }
        text
    }

    /// The QEMU monitor socket of a profile: it is how the process is stopped
    /// gracefully instead of being killed.
    pub fn monitor_path() -> String {
        format!("{}/monitor.sock", crate::profile::STATE_DIR)
    }

    /// The log the guest's console is appended to.
    pub fn log_path() -> String {
        format!("{}/guest.log", crate::profile::STATE_DIR)
    }
}

/// Build the launch plan for a profile.
///
/// The guest is described as an ordinary PC with UEFI firmware: an AHCI
/// controller for the disk, a USB controller so the guest has a keyboard and a
/// mouse, and the passed-through card. Anything the guest could reach back into
/// the host with (a shared filesystem, a guest agent) is left out on purpose:
/// the point of the profile is that the two systems stay apart.
pub fn build(profile: &Profile) -> Command {
    let threads = profile.threads();
    let memory = profile.memory_mib();

    let cpu = if profile.hide_hypervisor {
        "host,kvm=off,hv_vendor_id=GenuineIntel"
    } else {
        "host,kvm=off"
    };

    let mut args: Vec<String> = vec![
        "-name".into(),
        profile.name.clone(),
        "-machine".into(),
        "q35,accel=kvm".into(),
        "-cpu".into(),
        cpu.into(),
        "-smp".into(),
        format!("{threads},cores={threads},threads=1,sockets=1"),
        "-m".into(),
        memory.to_string(),
        // The guest keeps its own copy of the firmware variables: letting it
        // write the host's ones is a reliable way to brick a boot.
        "-drive".into(),
        format!("if=pflash,format=raw,readonly=on,file={OVMF_CODE}"),
        "-drive".into(),
        format!("if=pflash,format=raw,file={OVMF_VARS}"),
        "-device".into(),
        "ich9-ahci,id=sata".into(),
        "-drive".into(),
        format!(
            "id=windows,if=none,file={},format=raw,cache=none,aio=native",
            profile.windows_disk
        ),
        "-device".into(),
        "ide-hd,drive=windows,bus=sata.2,bootindex=1".into(),
        "-device".into(),
        "usb-ehci,id=usb".into(),
        "-device".into(),
        "usb-tablet,bus=usb.0".into(),
        "-device".into(),
        "usb-kbd,bus=usb.0".into(),
        "-device".into(),
        format!("vfio-pci,host={},multifunction=on,x-vga=on", profile.gpu),
        "-monitor".into(),
        format!("unix:{},server,nowait", Command::monitor_path()),
        "-rtc".into(),
        "base=localtime,clock=host,driftfix=slew".into(),
    ];

    // The EFI System Partition is handed over read only: the guest only ever
    // reads its own boot manager from it.
    if !profile.windows_esp.is_empty() {
        args.push("-drive".into());
        args.push(format!(
            "id=esp,if=none,file={},format=raw,readonly=on",
            profile.windows_esp
        ));
    }

    match profile.display {
        Display::LookingGlass => {
            // The card belongs exclusively to the guest; the frames leave it
            // through the shared memory region the Looking Glass client reads.
            args.push("-display".into());
            args.push("none".into());
            args.push("-device".into());
            args.push("ivshmem-plain,memdev=looking-glass".into());
            args.push("-object".into());
            args.push(
                "memory-backend-file,id=looking-glass,size=64M,mem-path=/dev/kvmfr0,share=on"
                    .into(),
            );
        }
        Display::None => {
            args.push("-display".into());
            args.push("none".into());
        }
    }

    // Pinning is done by the supervisor, which knows the cores it left to the
    // host; the plan only carries the IO thread so that the disk backend never
    // competes with the guest for the same vCPU.
    args.push("-object".into());
    args.push("iothread,id=io".into());
    args.push("-device".into());
    args.push("virtio-blk-pci,drive=windows,iothread=io".into());

    Command {
        program: QEMU_BINARY.to_string(),
        args,
    }
}

/// Everything the plan needs that the machine does not currently provide.
///
/// An empty list means the profile can be started; the entries say what to
/// install or enable, in the order it has to be fixed.
pub fn preflight(profile: &Profile) -> Vec<String> {
    let mut problems = Vec::new();

    if !vfio::is_iommu_enabled() {
        problems.push(
            "the IOMMU is not active: add intel_iommu=on to the kernel command line and reboot"
                .to_string(),
        );
    }

    if !std::path::Path::new("/dev/kvm").exists() {
        problems.push("/dev/kvm is missing: the kvm module is not loaded".to_string());
    }

    if !std::path::Path::new(OVMF_CODE).exists() {
        problems.push(format!(
            "the UEFI firmware image {OVMF_CODE} is missing: install edk2-ovmf"
        ));
    }

    if !std::path::Path::new(profile.windows_disk.as_str()).exists() {
        problems.push(format!(
            "the Windows partition {} does not exist",
            profile.windows_disk
        ));
    }

    match vfio::describe(&profile.gpu) {
        Ok(function) => {
            if !function.is_isolated() {
                problems.push(format!(
                    "{} shares its IOMMU group with {} other function(s); passing it through \
                     would take them along",
                    function.address,
                    function.group_peers.len()
                ));
            }
        }
        Err(err) => problems.push(err.to_string()),
    }

    if profile.display == Display::LookingGlass && !std::path::Path::new("/dev/kvmfr0").exists()
    {
        problems.push(
            "/dev/kvmfr0 is missing: install looking-glass-module-dkms, load the kvmfr module and \
             start the Looking Glass client to see the guest in a window (the HDMI output still \
             works without it)"
                .to_string(),
        );
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::MemoryPolicy;

    fn profile() -> Profile {
        Profile {
            name: "helm-windows".into(),
            gpu: "0000:01:00.0".into(),
            windows_disk: "/dev/nvme0n1p3".into(),
            windows_esp: "/dev/nvme0n1p1".into(),
            guest_threads: 12,
            guest_affinity: Some("0-11".into()),
            memory: MemoryPolicy::Fixed(12288),
            display: Display::None,
            hide_hypervisor: true,
        }
    }

    fn value_after(command: &Command, flag: &str) -> String {
        let index = command
            .args
            .iter()
            .position(|arg| arg == flag)
            .expect("the plan carries the flag");
        command.args[index + 1].clone()
    }

    #[test]
    fn the_plan_passes_the_card_through() {
        let command = build(&profile());
        assert!(command
            .args
            .iter()
            .any(|arg| arg == "vfio-pci,host=0000:01:00.0,multifunction=on,x-vga=on"));
    }

    #[test]
    fn the_host_firmware_variables_are_read_only() {
        let command = build(&profile());
        assert!(command
            .args
            .iter()
            .any(|arg| arg.contains(&format!("readonly=on,file={OVMF_CODE}"))));
        assert!(command
            .args
            .iter()
            .any(|arg| arg == &format!("if=pflash,format=raw,file={OVMF_VARS}")));
    }

    #[test]
    fn the_guest_gets_the_cores_of_the_profile() {
        let command = build(&profile());
        assert_eq!(
            value_after(&command, "-smp"),
            "12,cores=12,threads=1,sockets=1"
        );
    }

    #[test]
    fn the_memory_of_the_profile_is_the_memory_of_the_guest() {
        let command = build(&profile());
        assert_eq!(value_after(&command, "-m"), "12288");
    }

    #[test]
    fn the_plan_never_lets_the_guest_see_a_host_directory() {
        let command = build(&profile());
        assert!(!command.args.iter().any(|arg| arg.contains("virtiofs")));
        assert!(!command.args.iter().any(|arg| arg.contains("virtio-9p")));
    }

    #[test]
    fn a_looking_glass_profile_creates_the_shared_memory_device() {
        let mut profile = profile();
        profile.display = Display::LookingGlass;
        let command = build(&profile);
        assert!(command
            .args
            .iter()
            .any(|arg| arg == "ivshmem-plain,memdev=looking-glass"));
    }

    #[test]
    fn hiding_the_hypervisor_changes_the_cpu_model() {
        let mut profile = profile();
        profile.hide_hypervisor = false;
        assert_eq!(value_after(&build(&profile), "-cpu"), "host,kvm=off");
    }

    #[test]
    fn a_rendered_plan_quotes_its_arguments() {
        let command = Command {
            program: QEMU_BINARY.into(),
            args: vec!["-name".into(), "two words".into()],
        };
        assert_eq!(command.render(), "qemu-system-x86_64 -name 'two words'");
    }
}
