//! The Helm user interface.
//!
//! The interface itself runs without any privileges: it talks to the
//! privileged helper over a local socket and shows what the helper finds.

mod helper;

use helm_protocol::{DetectionReport, HelperInfo, Request};
use serde::Serialize;

/// The status of the helper, as shown in the header.
#[derive(Debug, Serialize)]
struct HelperStatus {
    reachable: bool,
    version: Option<String>,
    uefi: bool,
    can_write_boot_variables: bool,
    message: String,
}

/// The answer of a boot switch.
#[derive(Debug, Serialize)]
struct SwitchResult {
    os_id: String,
    label: String,
}

/// Ask the helper whether it is alive.
#[tauri::command]
fn helper_status() -> HelperStatus {
    match helper::request(&Request::Ping) {
        Ok(helm_protocol::Response::Ok { data }) => {
            let info: HelperInfo = serde_json::from_value(data).unwrap_or(HelperInfo {
                capabilities: helm_protocol::HelperCapabilities {
                    version: "unknown".into(),
                    can_write_boot_variables: false,
                    can_reboot: false,
                    uefi: false,
                },
                peer_uid: None,
            });
            HelperStatus {
                reachable: true,
                version: Some(info.capabilities.version.clone()),
                uefi: info.capabilities.uefi,
                can_write_boot_variables: info.capabilities.can_write_boot_variables,
                message: if info.capabilities.uefi {
                    "The firmware variables are available.".into()
                } else {
                    "This machine did not boot through UEFI: only a legacy BIOS boot is possible."
                        .into()
                },
            }
        }
        Ok(helm_protocol::Response::Error { message, .. }) => HelperStatus {
            reachable: false,
            version: None,
            uefi: false,
            can_write_boot_variables: false,
            message,
        },
        Err(message) => HelperStatus {
            reachable: false,
            version: None,
            uefi: false,
            can_write_boot_variables: false,
            message,
        },
    }
}

/// Run the detection and return the report.
#[tauri::command]
fn detect_oses() -> Result<DetectionReport, String> {
    let data = helper::request_data(&Request::Detect)?;
    serde_json::from_value(data).map_err(|err| format!("malformed report: {err}"))
}

/// Point the firmware at another operating system.
#[tauri::command]
fn set_boot_next(os_id: String) -> Result<SwitchResult, String> {
    let data = helper::request_data(&Request::SetBootNext {
        os_id: os_id.clone(),
    })?;
    Ok(SwitchResult {
        os_id,
        label: data
            .get("label")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// Reboot the machine.
#[tauri::command]
fn reboot() -> Result<(), String> {
    helper::request_data(&Request::Reboot).map(|_| ())
}

/// The pending boot target, if there is one.
#[tauri::command]
fn boot_next() -> Result<serde_json::Value, String> {
    helper::request_data(&Request::GetBootNext)
}

/// Run the interface.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            helper_status,
            detect_oses,
            set_boot_next,
            reboot,
            boot_next
        ])
        .run(tauri::generate_context!())
        .expect("the interface could not be started");
}
