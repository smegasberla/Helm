//! Integration with the service manager of the platform.
//!
//! * Linux: the helper is started by systemd. It announces its readiness
//!   over the `NOTIFY_SOCKET` and stops when systemd asks it to.
//! * Windows: the helper runs as a Windows service and reacts to the
//!   control messages of the service control manager.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set to true when the helper should stop.
pub static STOPPING: AtomicBool = AtomicBool::new(false);

/// Whether the helper should stop.
pub fn should_stop() -> bool {
    STOPPING.load(Ordering::Relaxed)
}

/// Ask the helper to stop.
pub fn request_stop() {
    STOPPING.store(true, Ordering::Relaxed);
}

/// The name of the Windows service.
#[cfg(windows)]
pub const SERVICE_NAME: &str = "HelmHelper";

/// Prepare the process for the service manager of the platform.
pub fn prepare() {
    #[cfg(unix)]
    platform::install_signal_handlers();
}

/// Announce that the helper is ready.
pub fn notify_ready() {
    #[cfg(unix)]
    platform::notify_ready();
    #[cfg(windows)]
    platform::set_status(platform::SERVICE_STATUS_RUNNING);
}

/// Announce that the helper is stopping.
pub fn notify_stopping() {
    #[cfg(unix)]
    platform::notify_stopping();
    #[cfg(windows)]
    platform::set_status(platform::SERVICE_STATUS_STOPPED);
}

/// Run the helper as a Windows service. The body is the work of the
/// service; it returns when the service is stopped.
#[cfg(windows)]
pub fn run_as_service(body: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()> {
    platform::run_as_service(body)
}

#[cfg(unix)]
mod platform {
    /// Tell systemd that the service is ready.
    pub fn notify_ready() {
        notify("READY=1\nSTATUS=Helm helper is listening");
    }

    /// Tell systemd that the service is stopping.
    pub fn notify_stopping() {
        notify("STOPPING=1\nSTATUS=Helm helper is stopping");
    }

    /// Send a message to the systemd notification socket.
    fn notify(message: &str) {
        let Ok(address) = std::env::var("NOTIFY_SOCKET") else {
            return;
        };
        // The address may start with an "@" for an abstract socket.
        let address = if let Some(stripped) = address.strip_prefix('@') {
            format!("\0{stripped}")
        } else {
            address
        };
        let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() else {
            return;
        };
        let _ = socket.connect(&address);
        let _ = socket.send(message.as_bytes());
    }

    /// Install the signal handlers that stop the helper.
    pub fn install_signal_handlers() {
        // SAFETY: the handler only sets an atomic flag, which is async
        // signal safe.
        unsafe {
            libc::signal(
                libc::SIGTERM,
                handle_stop as *const () as libc::sighandler_t,
            );
            libc::signal(libc::SIGINT, handle_stop as *const () as libc::sighandler_t);
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
    }

    extern "C" fn handle_stop(_signal: libc::c_int) {
        super::request_stop();
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::Mutex;

    use windows_sys::Win32::Foundation::FALSE;
    use windows_sys::Win32::System::Service::{
        RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
        SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_STATUS, SERVICE_STATUS_PENDING,
        SERVICE_STATUS_RUNNING, SERVICE_STATUS_STOPPED, SERVICE_WIN32_OWN_PROCESS,
        SP_SERVICE_STATUS,
    };

    pub const SERVICE_STATUS_RUNNING: u32 = 4;
    pub const SERVICE_STATUS_STOPPED: u32 = 1;
    pub const SERVICE_STATUS_PENDING: u32 = 2;

    /// The handle of the service status registration.
    static SERVICE_STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);

    /// The work of the service, handed over by `run_as_service`.
    static BODY: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>> = Mutex::new(None);

    #[repr(C)]
    struct SERVICE_TABLE_ENTRY {
        lpServiceName: *const u16,
        lpServiceProc: Option<unsafe extern "system" fn(u32, *mut u32) -> u32>,
    }

    /// Run the body as a Windows service.
    pub fn run_as_service(body: Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()> {
        let name: Vec<u16> = super::SERVICE_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        *BODY.lock().unwrap() = Some(body);

        // SAFETY: the handler only sets a flag.
        let ok = unsafe {
            RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handle_control), ptr::null_mut())
        };
        if ok == FALSE {
            return Err(std::io::Error::last_os_error());
        }

        let mut table = [
            SERVICE_TABLE_ENTRY {
                lpServiceName: name.as_ptr(),
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRY {
                lpServiceName: ptr::null(),
                lpServiceProc: None,
            },
        ];

        // SAFETY: the table is a valid array of service entries.
        let ok = unsafe { StartServiceCtrlDispatcherW(table.as_mut_ptr() as *mut _) };
        if ok == FALSE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut u32) -> u32 {
        // Take the work of the service and run it.
        let body = BODY.lock().unwrap().take();
        if let Some(body) = body {
            body();
        }
        0
    }

    unsafe extern "system" fn handle_control(
        control: u32,
        _event_type: u32,
        _event_data: *mut u8,
        _context: *mut c_void,
    ) -> u32 {
        match control {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                set_status(SERVICE_STATUS_PENDING);
                super::request_stop();
            }
            _ => {}
        }
        0
    }

    /// Report the state of the service to the service control manager.
    pub fn set_status(state: u32) {
        let handle = SERVICE_STATUS_HANDLE.load(Ordering::Relaxed);
        if handle == 0 {
            return;
        }
        let mut status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: 0,
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: 0,
        };
        // SAFETY: the handle was returned by RegisterServiceCtrlHandlerExW.
        unsafe {
            SetServiceStatus(handle as *mut _, &mut status as *mut SP_SERVICE_STATUS);
        }
    }
}
