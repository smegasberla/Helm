//! The Windows implementation of the transport: a named pipe.
//!
//! The pipe is created with a security descriptor that lets the local
//! administrators and the system account talk to it. Every request is
//! checked against the token of the client, so a client that is not an
//! administrator may ask for a detection but not for a boot switch.

use std::ffi::c_void;
use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, GetTokenInformation, TokenGroups, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, TOKEN_QUERY,
};

use crate::PeerCredentials;

/// Security descriptor of the pipe.
///
/// * `SY` the local system account (the service itself),
/// * `BA` the local administrators,
/// * `AU` every authenticated user, so that the unprivileged GUI can ask
///   for a detection. The privileges are checked per request.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";

/// `WinBuiltinAdministratorsSid`.
const WIN_BUILTIN_ADMINISTRATORS_SID: i32 = 9;

const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const INVALID_HANDLE: HANDLE = -1isize as HANDLE;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A security descriptor built from an SDDL string.
struct SecurityDescriptor {
    descriptor: *mut c_void,
    attributes: SECURITY_ATTRIBUTES,
}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text = wide(sddl);
        let mut descriptor: *mut c_void = ptr::null_mut();
        let mut size = 0u32;
        // SAFETY: the function fills the descriptor and reports its size.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1, // SDDL_REVISION_1
                &mut descriptor,
                &mut size,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        Ok(Self {
            descriptor,
            attributes,
        })
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was created by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(self.descriptor as _);
        }
    }
}

pub struct Listener {
    name: Vec<u16>,
    security: SecurityDescriptor,
}

impl Listener {
    pub fn bind(path: &str) -> io::Result<Self> {
        let security = SecurityDescriptor::from_sddl(PIPE_SDDL)?;
        Ok(Self {
            name: wide(path),
            security,
        })
    }

    /// Create a pipe instance that clients can connect to.
    pub fn create_instance(&self) -> io::Result<PipeStream> {
        // SAFETY: the pipe name is a NUL terminated wide string and the
        // security attributes outlive the call.
        let handle = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                0,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &self.security.attributes,
            )
        };
        if handle == INVALID_HANDLE {
            return Err(io::Error::last_os_error());
        }
        Ok(PipeStream {
            handle,
            peer: PeerCredentials::default(),
        })
    }

    /// Wait until a client connects to the pipe instance.
    pub fn wait_for_client(&self, stream: &mut PipeStream) -> io::Result<PeerCredentials> {
        // SAFETY: the handle was created by `create_instance`.
        let connected = unsafe { ConnectNamedPipe(stream.handle, ptr::null_mut()) };
        if connected == 0 {
            // The client may have connected between the creation of the
            // instance and this call; that is not an error.
            let code = unsafe { GetLastError() };
            if code != ERROR_PIPE_CONNECTED {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
        stream.peer = client_credentials(stream.handle);
        Ok(stream.peer)
    }

    pub fn accept(&self) -> io::Result<(PipeStream, PeerCredentials)> {
        let mut stream = self.create_instance()?;
        let peer = self.wait_for_client(&mut stream)?;
        Ok((stream, peer))
    }
}

pub struct Stream {
    pub stream: PipeStream,
    pub peer: PeerCredentials,
}

impl crate::Connection for Stream {
    fn peer(&self) -> crate::PeerCredentials {
        self.peer.clone()
    }
}

impl crate::Connection for Stream {
    fn peer(&self) -> crate::PeerCredentials {
        self.peer.clone()
    }
}

impl io::Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// A connected named pipe.
pub struct PipeStream {
    pub handle: HANDLE,
    pub peer: PeerCredentials,
}

impl PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        // SAFETY: the buffer is valid for the duration of the call.
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: the buffer is valid for the duration of the call.
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: the handle is a valid pipe handle.
        if unsafe { FlushFileBuffers(self.handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this object.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

pub fn connect(path: &str) -> io::Result<(PipeStream, PeerCredentials)> {
    let name = wide(path);
    let mut handle = INVALID_HANDLE;
    // A pipe that is being created can refuse the connection for a moment.
    for _ in 0..50 {
        // SAFETY: the name is a NUL terminated wide string.
        handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE {
            break;
        }
        // SAFETY: the name is a NUL terminated wide string.
        unsafe {
            WaitNamedPipeW(name.as_ptr(), 100);
        }
    }
    if handle == INVALID_HANDLE {
        let code = unsafe { GetLastError() };
        if code == ERROR_PIPE_BUSY {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the helper is busy",
            ));
        }
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let peer = client_credentials(handle);
    Ok((PipeStream { handle, peer }, peer))
}

/// The credentials of the client of a pipe.
fn client_credentials(handle: HANDLE) -> PeerCredentials {
    let mut credentials = PeerCredentials::default();
    let mut pid = 0u32;
    // SAFETY: the handle is a connected pipe.
    if unsafe { GetNamedPipeClientProcessId(handle, &mut pid) } == 0 {
        return credentials;
    }
    credentials.pid = Some(pid);
    credentials.is_admin = is_admin(pid);
    credentials
}

/// True when the process is a member of the local administrators.
fn is_admin(pid: u32) -> bool {
    // SAFETY: the process handle is closed at the end of the function.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut token: HANDLE = INVALID_HANDLE;
    // SAFETY: the token handle is closed at the end of the function.
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    unsafe { CloseHandle(process) };
    if opened == 0 {
        return false;
    }
    let member = check_group_membership(token);
    unsafe { CloseHandle(token) };
    member
}

/// True when the token contains the administrators group.
fn check_group_membership(token: HANDLE) -> bool {
    let mut size = 0u32;
    // SAFETY: a first call with a null buffer returns the needed size.
    unsafe {
        GetTokenInformation(token, TokenGroups, ptr::null_mut(), 0, &mut size);
    }
    if size == 0 {
        return false;
    }
    let mut buffer = vec![0u8; size as usize];
    // SAFETY: the buffer is large enough for the token information.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            buffer.as_mut_ptr() as *mut c_void,
            size,
            &mut size,
        )
    };
    if ok == 0 {
        return false;
    }

    let mut sid_buffer = [0u8; 68];
    let mut sid_size = 0u32;
    // SAFETY: the buffer is large enough for a well known SID.
    let created = unsafe {
        CreateWellKnownSid(
            WIN_BUILTIN_ADMINISTRATORS_SID,
            ptr::null_mut(),
            sid_buffer.as_mut_ptr() as *mut _,
            &mut sid_size,
        )
    };
    if created == 0 {
        return false;
    }
    let mut is_member = 0i32;
    // SAFETY: the SID was created above and the token is valid.
    let checked =
        unsafe { CheckTokenMembership(token, sid_buffer.as_ptr() as *const _, &mut is_member) };
    if checked == 0 {
        // Fall back to a manual scan of the group list.
        return groups_contain_administrators(&buffer, &sid_buffer);
    }
    is_member != 0
}

/// Scan the group list of a token for the administrators SID.
fn groups_contain_administrators(buffer: &[u8], administrators: &[u8]) -> bool {
    // The structure is a count followed by the group entries; the first entry
    // is the user id and has no attribute and SID.
    if buffer.len() < 4 {
        return false;
    }
    let count = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    let mut offset = 4usize;
    for _ in 0..count {
        if offset + 8 > buffer.len() {
            break;
        }
        let attributes = u32::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]);
        let sid_len = u32::from_le_bytes([
            buffer[offset + 4],
            buffer[offset + 5],
            buffer[offset + 6],
            buffer[offset + 7],
        ]) as usize;
        let sid_start = offset + 8;
        if sid_start + sid_len > buffer.len() {
            break;
        }
        if attributes != 0
            && attributes != 0x8000_0000
            && sid_len == administrators.len()
            && buffer[sid_start..sid_start + sid_len] == administrators[..sid_len]
        {
            return true;
        }
        offset = sid_start + sid_len;
        // The entries are aligned to 8 bytes.
        offset = (offset + 7) & !7;
    }
    false
}
