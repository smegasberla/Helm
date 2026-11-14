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
    /// True when the card already belonged to `vfio-pci` before the guest was
    /// started, which is what the dedicated boot entry produces.
    bound_at_boot: bool,
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
            // entries. The copy is made from the empty template, not from the
            // firmware image, which carries no writable store of its own.
            std::fs::copy(profile::OVMF_VARS_TEMPLATE, profile::OVMF_VARS)
                .map_err(StartError::Io)?;
        }

        // A card that the boot itself reserved for `vfio-pci` was never owned by
        // a host driver, so there is nothing to unbind and nothing to give back.
        // Recording it here keeps the way out of the session from handing a
        // guest-owned card back to a driver that no longer manages it.
        let bound_at_boot = vfio::current_driver(&plan.profile.gpu).as_deref()
            == Some(vfio::VFIO_DRIVER);

        vfio::bind(&plan.profile.gpu).map_err(|err| StartError::Vfio(err.to_string()))?;

        let mut child =
            spawn(&plan.command, plan.profile.guest_affinity.as_deref()).map_err(StartError::Io)?;

        // A guest that exits at once is the usual outcome of a card that never
        // made it to vfio-pci or a disk QEMU refuses. Reporting it here keeps the
        // interface from announcing a session that is already gone, and gives
        // the card back on the way out.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if let Ok(Some(status)) = child.try_wait() {
            if !bound_at_boot {
                let driver = plan
                    .released_driver()
                    .unwrap_or(vfio::HOST_DRIVER)
                    .to_string();
                let _ = vfio::release(&plan.profile.gpu, &driver);
            }
            return Err(StartError::Io(std::io::Error::other(format!(
                "the guest exited at once ({status}): {}",
                log_tail()
            ))));
        }

        Ok(Self {
            plan,
            bound_at_boot,
            child,
        })
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

        if self.bound_at_boot {
            // The card has to stay on `vfio-pci` for the rest of this boot: the
            // whole point of the dedicated entry is that no hand-over, and no
            // unbind that could wedge the driver core, ever happens.
            return stopped;
        }

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
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(qemu::Command::log_path())?;
    let errors = log.try_clone()?;

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
    // QEMU explains a refusal on its stderr; without the log a launch that failed
    // is indistinguishable from one that is merely slow.
    process
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(errors))
        .spawn()
}

/// The last lines QEMU wrote, used to explain a launch that died at once.
fn log_tail() -> String {
    let text = std::fs::read_to_string(qemu::Command::log_path()).unwrap_or_default();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(6)..].join(" | ")
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
