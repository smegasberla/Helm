//! The privileged helper of Helm.
//!
//! The helper does the work that needs root rights:
//!
//! * enumerating the physical disks and parsing their partition tables,
//! * inspecting the filesystems to find the installed operating systems,
//! * reading and writing the UEFI firmware variables,
//! * rebooting the machine.
//!
//! It talks to the unprivileged GUI over a local socket: a Unix domain
//! socket on Linux, a named pipe on Windows.

mod auth;
mod server;
mod service;

use std::io::Write;

use helm_ipc::{Connection, PeerCredentials};
use helm_protocol::Response;

use server::HelperState;

/// The version of the helper.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the helper is asked to do.
enum Mode {
    /// Serve the GUI over the socket.
    Serve,
    /// Run a detection once and print the report.
    Detect,
}

struct Args {
    socket: String,
    mode: Mode,
    disks: Vec<String>,
    show_version: bool,
    show_help: bool,
    install_service: bool,
    uninstall_service: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        socket: helm_protocol::helper_socket_path(),
        mode: Mode::Serve,
        disks: Vec::new(),
        show_version: false,
        show_help: false,
        install_service: false,
        uninstall_service: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--serve" => args.mode = Mode::Serve,
            "--detect" => args.mode = Mode::Detect,
            "--install-service" => args.install_service = true,
            "--uninstall-service" => args.uninstall_service = true,
            "--version" | "-V" => args.show_version = true,
            "--help" | "-h" => args.show_help = true,
            "--socket" => {
                args.socket = iter.next().ok_or("--socket needs a path")?;
            }
            "--disk" => {
                args.disks.push(iter.next().ok_or("--disk needs a path")?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        "Helm helper {VERSION}\n\
         \n\
         Usage:\n  \
         helm-helper [options]\n\
         \n\
         Options:\n  \
         --serve           Run the service that serves the GUI (the default)\n  \
         --detect          Run a detection and print the report as JSON (exit 1 on warnings)\n  \
         --install-service Register the helper as a Windows service\n  \
         --uninstall-service Remove the Windows service\n  \
         --socket <path>   Use another socket than {}\n  \
         --disk <path>     Use a disk image instead of the real disks\n  \
         --version         Print the version\n  \
         --help            Print this help",
        helm_protocol::HELPER_SOCKET
    );
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("helm-helper: {message}");
            std::process::exit(2);
        }
    };

    if args.show_help {
        print_help();
        return;
    }
    if args.show_version {
        println!("helm-helper {VERSION}");
        return;
    }
    if args.install_service {
        if let Err(err) = install_service() {
            eprintln!("helm-helper: {err}");
            std::process::exit(1);
        }
        return;
    }
    if args.uninstall_service {
        if let Err(err) = uninstall_service() {
            eprintln!("helm-helper: {err}");
            std::process::exit(1);
        }
        return;
    }

    match args.mode {
        Mode::Detect => {
            if let Err(message) = run_detection(&args.disks) {
                eprintln!("helm-helper: {message}");
                std::process::exit(1);
            }
        }
        Mode::Serve => {
            service::prepare();
            if let Err(err) = run_server(&args.socket, &args.disks) {
                eprintln!("helm-helper: {err}");
                std::process::exit(1);
            }
        }
    }
}

/// Run a detection and print the result.
///
/// The report is printed even when it contains warnings, so the caller can
/// always parse it. The error return value means that the report is not
/// complete, which lets the process end with a non zero exit code.
fn run_detection(disks: &[String]) -> Result<(), String> {
    let state = HelperState::new();
    let report = if disks.is_empty() {
        helm_detect::detect(state.store.as_deref())
    } else {
        helm_detect::detect_disks(disk_images(disks)?, state.store.as_deref())
    };
    let json = serde_json::to_string_pretty(&report)
        .map_err(|err| format!("cannot serialise the report: {err}"))?;
    println!("{json}");
    for warning in &report.warnings {
        eprintln!("helm-helper: warning: {warning}");
    }
    if report.warnings.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the detection finished with {} warning(s)",
            report.warnings.len()
        ))
    }
}

/// Register the helper as a Windows service.
#[cfg(windows)]
fn install_service() -> Result<(), String> {
    let binary = std::env::current_exe().map_err(|err| err.to_string())?;
    let binary = binary.to_string_lossy().to_string();
    let name = service::SERVICE_NAME;
    // Remove a service that is already there.
    let _ = std::process::Command::new("sc.exe")
        .args(["delete", name])
        .output();
    let status = std::process::Command::new("sc.exe")
        .args([
            "create",
            name,
            &format!("binPath= \"{binary}\" --serve"),
            "start= auto",
            "obj= LocalSystem",
            &format!("DisplayName= \"Helm Helper\""),
        ])
        .status()
        .map_err(|err| format!("sc.exe could not be run: {err}"))?;
    if !status.success() {
        return Err("the service could not be created (run this as administrator)".into());
    }
    let _ = std::process::Command::new("sc.exe")
        .args([
            "description",
            name,
            "Helm helper: finds the installed systems and switches the boot order",
        ])
        .status();
    println!("the service {name} was installed");
    Ok(())
}

/// Remove the Windows service.
#[cfg(windows)]
fn uninstall_service() -> Result<(), String> {
    let name = service::SERVICE_NAME;
    let _ = std::process::Command::new("sc.exe")
        .args(["stop", name])
        .status();
    let status = std::process::Command::new("sc.exe")
        .args(["delete", name])
        .status()
        .map_err(|err| format!("sc.exe could not be run: {err}"))?;
    if !status.success() {
        return Err("the service could not be removed".into());
    }
    println!("the service {name} was removed");
    Ok(())
}

#[cfg(not(windows))]
fn install_service() -> Result<(), String> {
    Err("the services are installed with the script in packaging/linux".into())
}

#[cfg(not(windows))]
fn uninstall_service() -> Result<(), String> {
    Err("the services are removed with the script in packaging/linux".into())
}

/// Turn the given paths into a list of disks.
///
/// A path that cannot be read is an error: dropping it silently would make
/// the report look like an empty machine instead of a broken argument.
fn disk_images(paths: &[String]) -> Result<Vec<helm_detect::RawDisk>, String> {
    let mut disks = Vec::with_capacity(paths.len());
    for path in paths {
        let metadata = std::fs::metadata(path)
            .map_err(|err| format!("cannot read the disk image {path}: {err}"))?;
        if !metadata.is_file() {
            return Err(format!("the disk image {path} is not a regular file"));
        }
        disks.push(helm_detect::RawDisk {
            path: path.clone(),
            model: std::path::Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone()),
            serial: None,
            size_bytes: metadata.len(),
            sector_size: 512,
            removable: false,
        });
    }
    Ok(disks)
}

/// The server loop.
fn run_server(socket: &str, disks: &[String]) -> std::io::Result<()> {
    let listener = helm_ipc::Listener::bind(socket)?;
    log::info!("the helper is listening on {socket}");
    service::notify_ready();

    let mut state = HelperState::new();
    if !disks.is_empty() {
        state.disks = Some(disk_images(disks).map_err(std::io::Error::other)?);
    }
    while !service::should_stop() {
        let mut connection = match listener.accept() {
            Ok(connection) => connection,
            Err(err) => {
                log::error!("cannot accept a connection: {err}");
                continue;
            }
        };
        let peer = connection.peer();
        log::info!("a client connected: {peer:?}");
        if let Err(err) = serve(&mut connection, &mut state, &peer) {
            log::warn!("the request failed: {err}");
        }
    }

    service::notify_stopping();
    log::info!("the helper is stopping");
    Ok(())
}

/// Serve a single request.
fn serve(
    connection: &mut Box<dyn Connection>,
    state: &mut HelperState,
    peer: &PeerCredentials,
) -> std::io::Result<()> {
    let frame = match helm_ipc::read_frame(connection.as_mut()) {
        Ok(frame) => frame,
        Err(err) => {
            // The client may have closed the connection without a request.
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(err);
        }
    };
    let response = match helm_protocol::decode_request(&frame) {
        Ok(request) => server::handle(request, state, peer),
        Err(err) => Response::error(format!("malformed request: {err}"), Some("bad_request")),
    };
    let payload = helm_protocol::encode_response(&response)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    helm_ipc::send_frame(connection.as_mut(), &payload)?;
    connection.flush()
}
