//! The description of the gaming profile.
//!
//! Everything the launcher needs is data, so the profile can be inspected,
//! edited and stored without touching the code. The values the machine can
//! measure itself (the cores, the memory, the cards) are filled in from the
//! live system instead of being guessed at compile time.

use serde::{Deserialize, Serialize};

use crate::topology::CpuTopology;

/// Where a QEMU process of this profile keeps its writable state.
pub const STATE_DIR: &str = "/var/lib/helm";

/// The firmware variables of the guest, kept apart from the ones of the host.
pub const OVMF_VARS: &str = "/var/lib/helm/ovmf_vars.fd";

/// The shared firmware image the guest starts from.
pub const OVMF_CODE: &str = "/usr/share/edk2/x64/OVMF_CODE.4m.fd";

/// The partition the Windows installation lives on.
pub const DEFAULT_WINDOWS_DISK: &str = "/dev/nvme0n1p3";

/// The EFI System Partition that carries the Windows boot manager.
pub const DEFAULT_WINDOWS_ESP: &str = "/dev/nvme0n1p1";

/// How the pixels of the guest reach the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Display {
    /// Looking Glass reads the guest framebuffer out of shared memory and shows
    /// it in a Linux window. The card stays passed through, only the transport
    /// is shared.
    LookingGlass,
    /// No window at all: the card drives the monitor plugged into its own port.
    None,
}

/// How much memory the guest gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPolicy {
    /// Half of the memory that is available right now, rounded down to GiB.
    Half,
    /// Everything above the given reserve, in GiB, kept for the host.
    Reserve(u64),
    /// An explicit size in MiB.
    Fixed(u64),
}

/// The complete gaming profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Human readable name, used as the QEMU process name.
    pub name: String,
    /// The PCI function handed to the guest, e.g. `0000:01:00.0`.
    pub gpu: String,
    /// The block device holding the Windows installation.
    pub windows_disk: String,
    /// The EFI System Partition the guest boots from.
    pub windows_esp: String,
    /// How many cores, in threads, the guest gets.
    pub guest_threads: usize,
    /// The CPUs to pin the guest to when the profile pins them.
    pub guest_affinity: Option<String>,
    /// How much memory the guest gets.
    pub memory: MemoryPolicy,
    /// The transport that brings the guest's pixels to the host.
    pub display: Display,
    /// Hide the KVM signature from the guest.
    ///
    /// Some games refuse to start when they can tell they run virtualised.
    /// Hiding the signature is not a guarantee: kernel level anti-cheat still
    /// detects the hypervisor, and the honest answer for those titles is to
    /// boot Windows natively instead.
    pub hide_hypervisor: bool,
}

impl Profile {
    /// The profile that fits the running machine.
    pub fn detect() -> Self {
        let topology = CpuTopology::read();
        Self {
            name: "helm-windows".to_string(),
            gpu: "0000:01:00.0".to_string(),
            windows_disk: DEFAULT_WINDOWS_DISK.to_string(),
            windows_esp: DEFAULT_WINDOWS_ESP.to_string(),
            guest_threads: topology.guest_threads(),
            guest_affinity: Some(topology.guest_affinity()),
            memory: MemoryPolicy::Reserve(8),
            display: Display::LookingGlass,
            hide_hypervisor: true,
        }
    }

    /// The memory the guest should get, in MiB.
    ///
    /// The value is derived from what is available right now rather than from
    /// the total, so a profile created on a busy machine does not hand the
    /// guest memory the host has already spent.
    pub fn memory_mib(&self) -> u64 {
        let available = available_memory_mib();
        match self.memory {
            MemoryPolicy::Half => (available / 2).max(2048),
            MemoryPolicy::Reserve(gib) => available.saturating_sub(gib.saturating_mul(1024)),
            MemoryPolicy::Fixed(mib) => mib,
        }
    }

    /// The cores the guest gets, never zero.
    pub fn threads(&self) -> usize {
        self.guest_threads.max(1)
    }
}

/// The memory the host can hand out right now, in MiB.
pub fn available_memory_mib() -> u64 {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kib: u64 = rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse()
                .unwrap_or(0);
            if kib > 0 {
                return kib / 1024;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(policy: MemoryPolicy) -> Profile {
        Profile {
            name: "test".into(),
            gpu: "0000:01:00.0".into(),
            windows_disk: DEFAULT_WINDOWS_DISK.into(),
            windows_esp: DEFAULT_WINDOWS_ESP.into(),
            guest_threads: 12,
            guest_affinity: Some("0-11".into()),
            memory: policy,
            display: Display::LookingGlass,
            hide_hypervisor: true,
        }
    }

    #[test]
    fn a_profile_always_offers_at_least_one_thread() {
        let mut profile = profile(MemoryPolicy::Half);
        profile.guest_threads = 0;
        assert_eq!(profile.threads(), 1);
    }

    #[test]
    fn a_fixed_policy_is_taken_verbatim() {
        assert_eq!(profile(MemoryPolicy::Fixed(6144)).memory_mib(), 6144);
    }

    #[test]
    fn a_reserve_policy_never_goes_negative() {
        let profile = profile(MemoryPolicy::Reserve(u64::MAX));
        assert_eq!(profile.memory_mib(), 0);
    }

    #[test]
    fn half_of_the_machine_is_at_least_two_gib() {
        assert!(profile(MemoryPolicy::Half).memory_mib() >= 2048);
    }

    #[test]
    fn the_detected_profile_fits_this_machine() {
        let profile = Profile::detect();
        assert_eq!(profile.gpu, "0000:01:00.0");
        assert!(profile.threads() >= 1);
        assert!(!profile.windows_disk.is_empty());
    }

    #[test]
    fn a_profile_survives_a_round_trip() {
        let profile = profile(MemoryPolicy::Reserve(8));
        let json = serde_json::to_string(&profile).unwrap();
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, back);
    }
}
