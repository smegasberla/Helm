//! The local transport between the GUI and the privileged helper.
//!
//! * Linux: a Unix domain socket inside the systemd runtime directory.
//! * Windows: a named pipe in the `\\.\pipe` namespace.
//!
//! The transport only carries bytes; the framing and the payloads live in
//! [`helm_protocol`].

use std::io::{Read, Write};

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use unix as platform;
#[cfg(windows)]
pub use windows as platform;

/// A bidirectional connection.
pub trait Connection: Read + Write + Send {
    /// Identity of the peer on the other side of the connection.
    fn peer(&self) -> PeerCredentials;
}

/// What is known about the peer of a connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerCredentials {
    /// The user id of the peer (Linux).
    pub uid: Option<u32>,
    /// The primary group id of the peer (Linux).
    pub gid: Option<u32>,
    /// The supplementary groups of the peer (Linux).
    pub groups: Vec<u32>,
    /// The process id of the peer.
    pub pid: Option<u32>,
    /// True when the peer is a member of the local administrators (Windows).
    pub is_admin: bool,
}

impl PeerCredentials {
    /// True when the peer is the superuser.
    pub fn is_root(&self) -> bool {
        self.uid == Some(0)
    }
}

/// A listening socket.
pub struct Listener {
    inner: platform::Listener,
}

impl Listener {
    /// Bind the listening socket.
    pub fn bind(path: &str) -> std::io::Result<Self> {
        Ok(Self {
            inner: platform::Listener::bind(path)?,
        })
    }

    /// Wait for the next client.
    pub fn accept(&self) -> std::io::Result<Box<dyn Connection>> {
        let (stream, peer) = self.inner.accept()?;
        Ok(Box::new(platform::Stream { stream, peer }))
    }
}

/// Connect to a running helper.
pub fn connect(path: &str) -> std::io::Result<(Box<dyn Connection>, PeerCredentials)> {
    let (stream, peer) = platform::connect(path)?;
    let credentials = peer.clone();
    Ok((Box::new(platform::Stream { stream, peer }), credentials))
}

/// Read a single length prefixed frame.
pub fn read_frame(connection: &mut dyn Connection) -> std::io::Result<Vec<u8>> {
    helm_protocol::read_frame(connection)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))
}

/// Write a raw payload as one length prefixed frame.
pub fn send_frame(connection: &mut dyn Connection, payload: &[u8]) -> std::io::Result<()> {
    let mut buffer = Vec::with_capacity(payload.len() + 4);
    buffer.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buffer.extend_from_slice(payload);
    connection.write_all(&buffer)?;
    connection.flush()
}
