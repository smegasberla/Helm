//! The client that talks to the privileged helper.

use helm_protocol::{Request, Response};

/// The path of the socket of the helper.
fn socket_path() -> String {
    helm_protocol::helper_socket_path()
}

/// Send a request to the helper and wait for the answer.
pub fn request(request: &Request) -> Result<Response, String> {
    let (mut connection, peer) = helm_ipc::connect(&socket_path()).map_err(|err| {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            return format!(
                "the helper at {} refused the connection. The user has to be a member of \
                 the 'helm' group: log out and back in, or run 'newgrp helm'.",
                socket_path()
            );
        }
        format!(
            "the privileged helper is not reachable at {} ({err}). Is the Helm helper \
             service running?",
            socket_path()
        )
    })?;
    log::debug!("connected to the helper, peer: {peer:?}");

    let payload = helm_protocol::encode_request(request).map_err(|err| err.to_string())?;
    helm_ipc::send_frame(connection.as_mut(), &payload)
        .map_err(|err| format!("the request could not be sent to the helper: {err}"))?;

    let frame = helm_ipc::read_frame(connection.as_mut())
        .map_err(|err| format!("the helper did not answer: {err}"))?;
    helm_protocol::decode_response(&frame).map_err(|err| format!("malformed answer: {err}"))
}

/// Send a request and unwrap the payload of a successful answer.
pub fn request_data(payload: &Request) -> Result<serde_json::Value, String> {
    match request(payload)? {
        Response::Ok { data } => Ok(data),
        Response::Error { message, .. } => Err(message),
    }
}
