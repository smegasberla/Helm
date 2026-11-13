//! Wire protocol and shared data types for Helm.
//!
//! The GUI and the privileged helper are two processes that speak the small
//! request/response protocol defined here over a local socket
//! (Unix domain socket on Linux, named pipe on Windows).

pub mod codec;
pub mod types;

pub use codec::{
    decode_request, decode_response, encode_request, encode_response, read_frame, read_message,
    write_frame, CodecError,
};
pub use types::*;
