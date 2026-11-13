//! The Unix implementation of the transport: a Unix domain socket.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{chown, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::PeerCredentials;

/// Group that owns the socket.
const HELPER_GROUP: &str = "helm";

pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

impl Listener {
    pub fn bind(path: &str) -> io::Result<Self> {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // A socket left behind by a previous run would block the bind.
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        let inner = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o660))?;
        if let Some(gid) = group_gid(HELPER_GROUP) {
            // The socket is owned by the helper group so that the members of
            // the group can talk to the helper. This needs the CAP_CHOWN
            // capability, so the service has to keep it.
            if let Err(err) = chown(&path, Some(0), Some(gid)) {
                log::error!(
                    "the socket {} could not be given to the group {HELPER_GROUP}: {err}. \
                     Only the superuser can talk to the helper. Add CAP_CHOWN to the \
                     capability bounding set of the service.",
                    path.display()
                );
            }
        } else {
            log::warn!(
                "the group {HELPER_GROUP} does not exist, only the superuser can talk to \
                 the helper. Create the group and add the users to it."
            );
        }
        Ok(Self { inner, path })
    }

    pub fn accept(&self) -> io::Result<(UnixStream, PeerCredentials)> {
        let (stream, _) = self.inner.accept()?;
        let peer = peer_credentials(&stream);
        Ok((stream, peer))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct Stream {
    pub stream: UnixStream,
    pub peer: PeerCredentials,
}

impl crate::Connection for Stream {
    fn peer(&self) -> crate::PeerCredentials {
        self.peer.clone()
    }
}

impl std::io::Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl std::io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

pub fn connect(path: &str) -> io::Result<(UnixStream, PeerCredentials)> {
    let stream = UnixStream::connect(path)?;
    let peer = peer_credentials(&stream);
    Ok((stream, peer))
}

/// The credentials of the process on the other side of the socket.
///
/// `SO_PEERCRED` is used directly: the wrapper of the standard library is
/// still unstable.
fn peer_credentials(stream: &UnixStream) -> PeerCredentials {
    let mut credentials = PeerCredentials::default();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the buffer is large enough for the credential structure.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if result != 0 {
        return credentials;
    }
    credentials.uid = Some(cred.uid);
    credentials.gid = Some(cred.gid);
    credentials.pid = Some(cred.pid as u32);
    credentials.groups = supplementary_groups(cred.pid);
    credentials
}

/// Read the supplementary groups of a process from `/proc`.
fn supplementary_groups(pid: i32) -> Vec<u32> {
    let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return Vec::new();
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Groups:") {
            return rest
                .split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok())
                .collect();
        }
    }
    Vec::new()
}

/// Look up the gid of a group by name.
pub fn group_gid(name: &str) -> Option<u32> {
    let content = fs::read_to_string("/etc/group").ok()?;
    for line in content.lines() {
        let mut parts = line.split(':');
        let group_name = parts.next()?;
        if group_name != name {
            continue;
        }
        let _password = parts.next()?;
        let gid = parts.next()?;
        return gid.parse::<u32>().ok();
    }
    None
}

/// The gid of the group that is allowed to talk to the helper.
pub fn helper_group_gid() -> Option<u32> {
    group_gid(HELPER_GROUP)
}

/// Check whether the path is a socket that nobody listens on.
pub fn is_stale(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match UnixStream::connect(path) {
        Ok(_) => false,
        Err(err) => matches!(
            err.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
        ),
    }
}
