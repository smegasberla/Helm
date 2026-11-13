//! Authorization of the requests that arrive over the local socket.
//!
//! The socket itself is already protected: on Linux it belongs to the
//! `helm` group, on Windows the named pipe only accepts the administrators,
//! the system account and authenticated users. On top of that, the requests
//! that change the state of the machine (writing a firmware variable and
//! rebooting) are only accepted from a privileged peer.

use helm_ipc::PeerCredentials;

/// What a request is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// Read only.
    Detect,
    /// Changes the firmware.
    Switch,
    /// Reboots the machine.
    Reboot,
}

/// Check whether a peer may perform a request.
pub fn authorize(peer: &PeerCredentials, kind: RequestKind) -> Result<(), String> {
    #[cfg(unix)]
    {
        let allowed = match kind {
            RequestKind::Detect => true,
            RequestKind::Switch | RequestKind::Reboot => is_privileged(peer),
        };
        if allowed {
            Ok(())
        } else {
            Err(format!(
                "the peer (uid {:?}) is not allowed to perform this operation",
                peer.uid
            ))
        }
    }
    #[cfg(windows)]
    {
        let allowed = match kind {
            RequestKind::Detect => true,
            RequestKind::Switch | RequestKind::Reboot => peer.is_admin,
        };
        if allowed {
            Ok(())
        } else {
            Err("administrator rights are required for this operation".to_string())
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (peer, kind);
        Ok(())
    }
}

/// True when the peer is the superuser or a member of the helper group.
#[cfg(unix)]
fn is_privileged(peer: &PeerCredentials) -> bool {
    if peer.is_root() {
        return true;
    }
    let Some(gid) = helm_ipc::platform::helper_group_gid() else {
        // Without a helper group only the superuser may change anything.
        return false;
    };
    peer.gid == Some(gid) || peer.groups.contains(&gid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_is_always_allowed() {
        let peer = PeerCredentials {
            uid: Some(1000),
            ..Default::default()
        };
        assert!(authorize(&peer, RequestKind::Detect).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_superuser_may_switch() {
        let peer = PeerCredentials {
            uid: Some(0),
            ..Default::default()
        };
        assert!(authorize(&peer, RequestKind::Switch).is_ok());
        assert!(authorize(&peer, RequestKind::Reboot).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_regular_user_may_not_switch() {
        let peer = PeerCredentials {
            uid: Some(1000),
            gid: Some(1000),
            groups: vec![1000],
            ..Default::default()
        };
        assert!(authorize(&peer, RequestKind::Switch).is_err());
        assert!(authorize(&peer, RequestKind::Reboot).is_err());
    }
}
