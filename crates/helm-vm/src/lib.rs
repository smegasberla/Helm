//! The gaming profile of Helm.
//!
//! The original purpose of Helm is to pick the system the machine boots into.
//! This crate adds the second way of reaching the other system: the dedicated
//! card is handed to a QEMU guest while the host keeps the panel, so Linux stays
//! usable while a game runs on Windows.
//!
//! Everything here builds a plan as data before anything is executed. That way
//! the launch line can be read, reviewed and tested without starting a virtual
//! machine, and the privileged parts (binding the card, writing under
//! `/var/lib/helm`) stay behind the helper that owns them.

pub mod profile;
pub mod qemu;
pub mod topology;
pub mod vfio;

pub use profile::{Display, MemoryPolicy, Profile};
pub use qemu::Command;
pub use topology::CpuTopology;
pub use vfio::PciFunction;

/// What a launch would do, without doing any of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The profile the plan was built from.
    pub profile: Profile,
    /// The QEMU process that would be started.
    pub command: Command,
    /// The card the guest would take and the driver it would give up.
    pub gpu: Option<PciFunction>,
    /// What still has to be installed or enabled before a launch can work.
    pub problems: Vec<String>,
}

impl Plan {
    /// True when nothing stands between the profile and a launch.
    pub fn is_ready(&self) -> bool {
        self.problems.is_empty()
    }

    /// Build the plan of the running machine.
    pub fn detect() -> Self {
        Self::for_profile(Profile::detect())
    }

    /// Build the plan of a given profile.
    pub fn for_profile(profile: Profile) -> Self {
        let problems = qemu::preflight(&profile);
        let gpu = vfio::describe(&profile.gpu).ok();
        let command = qemu::build(&profile);
        Self {
            profile,
            command,
            gpu,
            problems,
        }
    }

    /// The launch line, as it would be typed in a shell.
    pub fn render(&self) -> String {
        self.command.render()
    }

    /// The driver the host loses while the guest runs.
    pub fn released_driver(&self) -> Option<&str> {
        self.gpu.as_ref().and_then(|gpu| gpu.driver.as_deref())
    }
}

/// A running guest.
///
/// The process is started with the CPU mask of the performance cores and the
/// card bound to `vfio-pci`; dropping the session gives the card back, which is
/// the step that must never be skipped.
#[derive(Debug)]
pub struct Session {
    /// The plan the session was started from.
    pub plan: Plan,
    child: std::process::Child,
}

impl Session {
    /// True when the guest can be started on this machine right now.
    pub fn can_start(plan: &Plan) -> Result<(), String> {
        if plan.problems.is_empty() {
            Ok(())
        } else {
            Err(plan.problems.join("; "))
        }
    }

    /// Bind the card, pin the guest to the performance cores and start QEMU.
    ///
    /// This runs in the helper: binding a PCI function and writing the guest
    /// firmware image both need root rights.
    pub fn start(plan: Plan) -> Result<Self, StartError> {
        Self::can_start(&plan).map_err(StartError::NotReady)?;

        std::fs::create_dir_all(profile::STATE_DIR).map_err(StartError::Io)?;
        if !std::path::Path::new(profile::OVMF_VARS).exists() {
            // The guest needs its own copy of the variable store; sharing the
            // host's would let a Windows installer rewrite the host's boot
            // entries.
            std::fs::copy(profile::OVMF_CODE, profile::OVMF_VARS).map_err(StartError::Io)?;
        }

        vfio::bind(&plan.profile.gpu).map_err(|err| StartError::Vfio(err.to_string()))?;

        let child = spawn(&plan.command, plan.profile.guest_affinity.as_deref())
            .map_err(StartError::Io)?;

        Ok(Self { plan, child })
    }

    /// The process id of the guest.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// True while QEMU is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stop the guest and give the card back to the host.
    pub fn stop(&mut self) -> Result<(), StartError> {
        let stopped = self
            .child
            .kill()
            .and_then(|()| self.child.wait())
            .map(|_| ())
            .map_err(StartError::Io);

        let driver = self
            .plan
            .released_driver()
            .unwrap_or(vfio::HOST_DRIVER)
            .to_string();
        let released = vfio::release(&self.plan.profile.gpu, &driver)
            .map_err(|err| StartError::Vfio(err.to_string()));

        stopped.and(released)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The card has to go back even when the caller forgets to stop the
        // session: leaving it bound is what makes prime-run and CUDA vanish
        // until the next reboot.
        if self.is_running() {
            let _ = self.stop();
        }
    }
}

/// Start the QEMU process, constrained to the given CPU list.
///
/// The affinity is applied through `taskset` rather than with a raw
/// `sched_setaffinity`, so the constraint is already in place when QEMU
/// creates its own threads: a thread started outside the mask would put the
/// guest's work back onto the cores the host is supposed to keep.
fn spawn(command: &qemu::Command, affinity: Option<&str>) -> std::io::Result<std::process::Child> {
    let mut process = match affinity.filter(|list| !list.is_empty()) {
        Some(list) => {
            let mut process = std::process::Command::new("taskset");
            process
                .arg("-c")
                .arg(list)
                .arg(&command.program)
                .args(&command.args);
            process
        }
        None => {
            let mut process = std::process::Command::new(&command.program);
            process.args(&command.args);
            process
        }
    };
    process.spawn()
}

/// Everything that can go wrong between a plan and a running guest.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("the profile cannot be started yet: {0}")]
    NotReady(String),
    #[error("the card could not be bound to the guest: {0}")]
    Vfio(String),
    #[error("the guest could not be started: {0}")]
    Io(#[from] std::io::Error),
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_without_problems_is_ready() {
        let plan = Plan {
            profile: Profile::detect(),
            command: Command {
                program: "qemu".into(),
                args: Vec::new(),
            },
            gpu: None,
            problems: Vec::new(),
        };
        assert!(plan.is_ready());
        assert!(Session::can_start(&plan).is_ok());
    }

    #[test]
    fn a_plan_with_problems_refuses_to_start() {
        let mut plan = Plan {
            profile: Profile::detect(),
            command: Command {
                program: "qemu".into(),
                args: Vec::new(),
            },
            gpu: None,
            problems: Vec::new(),
        };
        plan.problems.push("no kvm".into());
        assert!(!plan.is_ready());
        assert!(Session::can_start(&plan).is_err());
    }

    #[test]
    fn a_detected_plan_names_the_card_of_this_machine() {
        let plan = Plan::detect();
        assert_eq!(plan.profile.gpu, "0000:01:00.0");
        assert!(plan.render().contains("qemu-system-x86_64"));
    }
}
