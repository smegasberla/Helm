//! The request handler of the helper.

use helm_boot::{BootSwitcher, FirmwareStore};
use helm_ipc::PeerCredentials;
use helm_vm::{Plan, Session};
use helm_protocol::{DetectionReport as Report, HelperCapabilities, HelperInfo, Request, Response};

use crate::auth::{authorize, RequestKind};

/// The state the helper keeps between the requests.
pub struct HelperState {
    /// The last detection report, so that the GUI can refer to an operating
    /// system by its id when it asks for a switch.
    pub report: Option<Report>,
    /// The firmware variable store, when it is accessible.
    pub store: Option<Box<dyn FirmwareStore>>,
    /// True when the machine booted through UEFI.
    pub uefi: bool,
    /// The disks to look at. When it is empty the real disks are used.
    pub disks: Option<Vec<helm_detect::RawDisk>>,
    /// The guest of the gaming profile, when one is running.
    pub session: Option<Session>,
}

impl HelperState {
    pub fn new() -> Self {
        Self {
            report: None,
            store: helm_boot::platform_store(),
            uefi: helm_boot::running_under_uefi(),
            disks: None,
            session: None,
        }
    }

    /// Run the detection and remember the result.
    pub fn detect(&mut self) -> Report {
        let report = match &self.disks {
            Some(disks) => helm_detect::detect_disks(disks.clone(), self.store.as_deref()),
            None => helm_detect::detect(self.store.as_deref()),
        };
        self.report = Some(report.clone());
        report
    }
}

impl Default for HelperState {
    fn default() -> Self {
        Self::new()
    }
}

/// The capabilities of this helper.
pub fn capabilities(state: &HelperState) -> HelperCapabilities {
    HelperCapabilities {
        version: env!("CARGO_PKG_VERSION").to_string(),
        can_write_boot_variables: state.store.is_some() && state.uefi,
        can_reboot: true,
        uefi: state.uefi,
    }
}

/// Handle a single request.
pub fn handle(request: Request, state: &mut HelperState, peer: &PeerCredentials) -> Response {
    let kind = match request {
        Request::Ping | Request::Detect | Request::GetBootNext => RequestKind::Detect,
        Request::VmPlan | Request::VmStatus => RequestKind::Detect,
        Request::SetBootNext { .. } | Request::VmStart | Request::VmStop => RequestKind::Switch,
        Request::Reboot => RequestKind::Reboot,
    };
    if let Err(message) = authorize(peer, kind) {
        return Response::error(message, Some("not_authorized"));
    }

    match request {
        Request::Ping => Response::ok(
            serde_json::to_value(HelperInfo {
                capabilities: capabilities(state),
                peer_uid: peer.uid,
            })
            .unwrap_or_default(),
        ),
        Request::Detect => {
            let report = state.detect();
            match serde_json::to_value(report) {
                Ok(value) => Response::ok(value),
                Err(err) => Response::error(format!("cannot serialise the report: {err}"), None),
            }
        }
        Request::GetBootNext => {
            let Some(store) = state.store.as_ref() else {
                return Response::error(
                    "the firmware variable store is not accessible",
                    Some("no_firmware"),
                );
            };
            let switcher = BootSwitcher::new(Box::new(StoreRef(store.as_ref())));
            match switcher.boot_next() {
                Ok(Some(data)) => Response::ok(serde_json::json!({
                    "pending": true,
                    "payload_hex": helm_boot::store::hex_encode(&data),
                })),
                Ok(None) => Response::ok(serde_json::json!({ "pending": false })),
                Err(err) => Response::error(err.to_string(), Some("firmware_error")),
            }
        }
        Request::SetBootNext { os_id } => {
            let Some(report) = state.report.as_ref() else {
                return Response::error(
                    "the detection has not run yet, ask for a detection first",
                    Some("no_report"),
                );
            };
            let Some(os) = report.oses.iter().find(|os| os.id == os_id) else {
                return Response::error(
                    format!("unknown operating system {os_id}"),
                    Some("unknown_os"),
                );
            };
            let Some(target) = os.boot.as_ref() else {
                return Response::error(
                    format!("{os_id} has no boot loader that UEFI can use"),
                    Some("not_bootable"),
                );
            };
            let Some(store) = state.store.as_ref() else {
                return Response::error(
                    "the firmware variable store is not accessible",
                    Some("no_firmware"),
                );
            };
            let switcher = BootSwitcher::new(Box::new(StoreRef(store.as_ref())));
            match switcher.set_boot_next(target) {
                Ok(()) => Response::ok(serde_json::json!({
                    "os_id": os_id,
                    "label": target.label,
                })),
                Err(err) => Response::error(err.to_string(), Some("firmware_error")),
            }
        }
        Request::Reboot => match helm_boot::reboot::reboot() {
            Ok(()) => Response::ok(serde_json::json!({ "rebooting": true })),
            Err(err) => Response::error(err.to_string(), Some("reboot_failed")),
        },
        Request::VmPlan => {
            let plan = Plan::detect();
            Response::ok(serde_json::json!({
                "profile": plan.profile,
                "command": plan.render(),
                "problems": plan.problems,
                "gpu": plan.gpu.map(|gpu| serde_json::json!({
                    "address": gpu.address,
                    "vendor_device": gpu.vendor_device,
                    "iommu_group": gpu.iommu_group,
                    "isolated": gpu.is_isolated(),
                    "driver": gpu.driver,
                })),
            }))
        }
        Request::VmStart => {
            if state.session.as_mut().map(Session::is_running).unwrap_or(false) {
                return Response::error("a guest is already running", Some("already_running"));
            }
            state.session = None;
            match Session::start(Plan::detect()) {
                Ok(session) => {
                    let pid = session.pid();
                    state.session = Some(session);
                    Response::ok(serde_json::json!({ "pid": pid }))
                }
                Err(err) => Response::error(err.to_string(), Some("vm_failed")),
            }
        }
        Request::VmStop => match state.session.take() {
            Some(mut session) => match session.stop() {
                Ok(()) => Response::ok(serde_json::json!({ "stopped": true })),
                Err(err) => Response::error(err.to_string(), Some("vm_failed")),
            },
            None => Response::ok(serde_json::json!({ "stopped": false })),
        },
        Request::VmStatus => {
            let running = state.session.as_mut().map(Session::is_running).unwrap_or(false);
            Response::ok(serde_json::json!({
                "running": running,
                "pid": state.session.as_ref().map(Session::pid),
            }))
        },
    }
}

/// A borrowed view on a firmware store, so that it can be used behind a
/// `Box<dyn FirmwareStore>`.
struct StoreRef<'a>(&'a dyn FirmwareStore);

impl FirmwareStore for StoreRef<'_> {
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, helm_boot::FirmwareError> {
        self.0.read(name)
    }

    fn write(&self, name: &str, data: &[u8]) -> Result<(), helm_boot::FirmwareError> {
        self.0.write(name, data)
    }

    fn delete(&self, name: &str) -> Result<bool, helm_boot::FirmwareError> {
        self.0.delete(name)
    }

    fn list(&self) -> Result<Vec<String>, helm_boot::FirmwareError> {
        self.0.list()
    }

    fn is_uefi(&self) -> bool {
        self.0.is_uefi()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helm_protocol::DetectedOs;

    #[test]
    fn ping_is_answered() {
        let mut state = HelperState::new();
        let peer = PeerCredentials {
            uid: Some(1000),
            ..Default::default()
        };
        let response = handle(Request::Ping, &mut state, &peer);
        match response {
            Response::Ok { data } => {
                assert!(data["capabilities"]["version"].as_str().is_some());
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_regular_user_may_not_switch() {
        let mut state = HelperState::new();
        state.report = Some(Report {
            oses: vec![DetectedOs {
                id: "os-0-1".into(),
                name: "Linux".into(),
                kind: helm_protocol::OsKind::Linux,
                disk_index: 0,
                disk_label: "disk".into(),
                partition_number: 1,
                partition_label: String::new(),
                fs: helm_protocol::FileSystemKind::Ext4,
                distro: None,
                icon: None,
                boot: None,
                evidence: Vec::new(),
                bootable: false,
            }],
            ..Default::default()
        });
        let peer = PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
            ..Default::default()
        };
        let response = handle(
            Request::SetBootNext {
                os_id: "os-0-1".into(),
            },
            &mut state,
            &peer,
        );
        match response {
            Response::Error { reason, .. } => assert_eq!(reason.as_deref(), Some("not_authorized")),
            other => panic!("unexpected response: {other:?}"),
        }
    }
}
