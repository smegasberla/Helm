// The graphical interface of Helm.
//
// The binary is a thin wrapper around the library: it only starts the
// interface, everything else happens in `helm_gui_lib`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(unix)]
    unprivileged::ensure();

    helm_gui_lib::run();
}

/// The interface never works with privileges, so a stray `sudo` is undone
/// before GTK is initialised.
///
/// The interface reads nothing from the disks or the firmware by itself: the
/// privileged helper does that over the local socket, and the socket accepts
/// the members of the `helm` group. Running as root therefore buys nothing and
/// actively breaks the start: sudo hands over an environment whose HOME and
/// XAUTHORITY belong to root, so the display server rejects the connection and
/// GTK aborts with "Failed to initialize GTK".
#[cfg(unix)]
mod unprivileged {
    use std::ffi::CString;

    pub fn ensure() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }

        let Some(user) = std::env::var("SUDO_USER").ok().filter(|name| !name.is_empty()) else {
            eprintln!(
                "helm-gui must not run as root: start it from your own session.\n\
                 The privileged work is done by the helm-helper service; Helm only needs the\n\
                 'helm' group membership, which the installer grants."
            );
            std::process::exit(1);
        };

        let uid = parse_id("SUDO_UID");
        let gid = parse_id("SUDO_GID");
        let home = home_of(&user).unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());

        // The environment is what the display server authorises: root's HOME
        // and a vanished user session are the two reasons GTK refuses to start.
        std::env::set_var("HOME", &home);
        std::env::set_var("USER", &user);
        std::env::set_var("LOGNAME", &user);
        if std::env::var_os("XAUTHORITY").is_none() {
            let candidate = format!("{home}/.Xauthority");
            if std::path::Path::new(&candidate).exists() {
                std::env::set_var("XAUTHORITY", candidate);
            }
        }
        recover_session_bus(uid);

        if let Some(name) = CString::new(user.as_bytes()).ok() {
            let base_gid = gid.unwrap_or(0);
            // SAFETY: initgroups and the two id changes below only ever lower
            // the privileges of this process, and all of them run while it is
            // still root; setgid has to happen before setuid.
            unsafe {
                libc::initgroups(name.as_ptr(), base_gid);
            }
        }
        if let Some(gid) = gid {
            // SAFETY: see above.
            unsafe {
                libc::setgid(gid);
            }
        }
        if let Some(uid) = uid {
            // SAFETY: see above; from here on the process holds the user's rights.
            unsafe {
                libc::setuid(uid);
            }
        }
    }

    fn parse_id(key: &str) -> Option<u32> {
        std::env::var(key).ok()?.trim().parse().ok()
    }

    /// The home directory of `user`, taken from the password database.
    fn home_of(user: &str) -> Option<String> {
        let name = CString::new(user.as_bytes()).ok()?;
        // SAFETY: getpwnam returns a pointer into a static buffer owned by libc;
        // it is read immediately and never freed here.
        unsafe {
            let entry = libc::getpwnam(name.as_ptr());
            if entry.is_null() {
                return None;
            }
            let dir = (*entry).pw_dir;
            if dir.is_null() {
                return None;
            }
            std::ffi::CStr::from_ptr(dir).to_str().ok().map(str::to_string)
        }
    }

    /// sudo resets the session variables, so a Wayland client would fall back
    /// to a display connection that was never authorised. The runtime directory
    /// of the invoking user is where the wayland socket still lives.
    fn recover_session_bus(uid: Option<u32>) {
        let Some(uid) = uid else { return };
        let runtime = format!("/run/user/{uid}");
        if !std::path::Path::new(&runtime).is_dir() {
            return;
        }
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            let socket = std::fs::read_dir(&runtime)
                .ok()
                .and_then(|dir| {
                    dir.filter_map(|entry| entry.ok())
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .find(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
                });
            if let Some(socket) = socket {
                std::env::set_var("WAYLAND_DISPLAY", socket);
            }
        }
    }
}
