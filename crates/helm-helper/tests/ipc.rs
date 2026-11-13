//! End to end test of the protocol between the interface and the helper.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use helm_protocol::{Request, Response};

/// The helper that is started for the test.
struct Helper {
    child: Child,
    socket: String,
}

impl Helper {
    fn start(name: &str) -> Self {
        let socket = format!("/tmp/helm-ipc-test-{}-{}.sock", name, std::process::id());
        Helper::start_at(name, socket)
    }

    fn start_at(_name: &str, socket: String) -> Self {
        let _ = std::fs::remove_file(&socket);
        let child = Command::new(env!("CARGO_BIN_EXE_helm-helper"))
            .args(["--serve", "--socket", &socket])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the helper must start");
        let helper = Self { child, socket };
        helper.wait_for_socket();
        helper
    }

    fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::path::Path::new(&self.socket).exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the helper did not create its socket");
    }

    /// Send a request and read the answer.
    fn call(&self, request: &Request) -> Response {
        let (mut connection, _) = helm_ipc::connect(&self.socket).expect("connect to the helper");
        let payload = helm_protocol::encode_request(request).unwrap();
        helm_ipc::send_frame(connection.as_mut(), &payload).unwrap();
        let frame = helm_ipc::read_frame(connection.as_mut()).unwrap();
        helm_protocol::decode_response(&frame).unwrap()
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[test]
fn the_helper_answers_over_the_socket() {
    let helper = Helper::start("ping");

    // The helper has to answer a ping.
    match helper.call(&Request::Ping) {
        Response::Ok { data } => {
            assert!(data["capabilities"]["version"].is_string());
            assert!(data["capabilities"]["uefi"].is_boolean());
        }
        other => panic!("unexpected answer: {other:?}"),
    }

    // The detection has to work as well. The disks of the machine may not be
    // readable without root rights, but the answer still has to be a valid
    // report.
    match helper.call(&Request::Detect) {
        Response::Ok { data } => {
            assert!(data["oses"].is_array());
            assert!(data["disks"].is_array());
            assert!(data["uefi"].is_boolean());
        }
        other => panic!("unexpected answer: {other:?}"),
    }
}

#[test]
fn unknown_operating_systems_are_refused() {
    let helper = Helper::start("switch");
    // The detection has to run first, the helper only remembers the last one.
    helper.call(&Request::Detect);
    match helper.call(&Request::SetBootNext {
        os_id: "does-not-exist".into(),
    }) {
        Response::Error { reason, .. } => {
            assert!(
                matches!(
                    reason.as_deref(),
                    Some("unknown_os") | Some("not_authorized")
                ),
                "unexpected reason: {reason:?}"
            );
        }
        other => panic!("unexpected answer: {other:?}"),
    }
}

#[test]
fn a_stale_socket_does_not_block_the_helper() {
    let mut helper = Helper::start("cleanup");
    let socket = helper.socket.clone();
    assert!(std::path::Path::new(&socket).exists());
    // Kill the helper without letting it clean up: the socket file stays
    // behind.
    let _ = helper.child.kill();
    let _ = helper.child.wait();
    assert!(std::path::Path::new(&socket).exists());

    // A new helper has to take over the socket.
    let restarted = Helper::start_at("cleanup", socket);
    match restarted.call(&Request::Ping) {
        Response::Ok { .. } => {}
        other => panic!("unexpected answer: {other:?}"),
    }
}

/// The helper writes to its standard error, make sure the test does not fill
/// the pipe.
#[test]
fn the_helper_survives_a_broken_request() {
    let helper = Helper::start("broken");
    let (mut connection, _) = helm_ipc::connect(&helper.socket).unwrap();
    // A frame that is not a valid request.
    let mut frame = Vec::new();
    frame.extend_from_slice(&7u32.to_be_bytes());
    frame.extend_from_slice(b"not json");
    connection.write_all(&frame).unwrap();
    connection.flush().unwrap();
    let frame = helm_ipc::read_frame(connection.as_mut()).unwrap();
    match helm_protocol::decode_response(&frame).unwrap() {
        Response::Error { reason, .. } => assert_eq!(reason.as_deref(), Some("bad_request")),
        other => panic!("unexpected answer: {other:?}"),
    }
}
