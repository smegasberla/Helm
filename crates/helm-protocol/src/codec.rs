//! Length prefixed framing for the helper <-> GUI protocol.
//!
//! Wire format:
//!
//! ```text
//! +--------------------+--------------------------+
//! | u32 BE length      | JSON payload (UTF-8)     |
//! +--------------------+--------------------------+
//! ```
//!
//! The framing is deliberately trivial: it works identically over a Unix
//! domain socket and over a Windows named pipe, and it is trivial to
//! implement in any language should the helper ever be re-used elsewhere.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::types::{Request, Response, MAX_FRAME_SIZE};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid utf-8 in frame payload")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("malformed json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame of {0} bytes exceeds the {1} byte limit")]
    FrameTooLarge(usize, usize),
}

/// Write a single frame containing the JSON encoding of `value`.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), CodecError> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(CodecError::FrameTooLarge(payload.len(), MAX_FRAME_SIZE));
    }
    let len = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len)?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// Read a single frame and return its raw payload.
pub fn read_frame<R: Read + ?Sized>(reader: &mut R) -> Result<Vec<u8>, CodecError> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(CodecError::FrameTooLarge(len, MAX_FRAME_SIZE));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

/// Read a frame and decode it as JSON.
pub fn read_message<R: Read + ?Sized, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<T, CodecError> {
    let payload = read_frame(reader)?;
    Ok(serde_json::from_slice(&payload)?)
}

/// Encode a request into a frame payload.
pub fn encode_request(req: &Request) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(req)?)
}

/// Decode a request from a frame payload.
pub fn decode_request(payload: &[u8]) -> Result<Request, CodecError> {
    Ok(serde_json::from_slice(payload)?)
}

/// Encode a response into a frame payload.
pub fn encode_response(resp: &Response) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(resp)?)
}

/// Decode a response from a frame payload.
pub fn decode_response(payload: &[u8]) -> Result<Response, CodecError> {
    Ok(serde_json::from_slice(payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Request;

    #[test]
    fn request_roundtrip() {
        let req = Request::SetBootNext {
            os_id: "os-0".into(),
        };
        let bytes = encode_request(&req).unwrap();
        assert_eq!(decode_request(&bytes).unwrap(), req);
    }

    #[test]
    fn framed_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Request::Ping).unwrap();
        write_frame(&mut buf, &Request::Reboot).unwrap();

        let mut cursor = &buf[..];
        let first: Request = read_message(&mut cursor).unwrap();
        assert_eq!(first, Request::Ping);
        let second: Request = read_message(&mut cursor).unwrap();
        assert_eq!(second, Request::Reboot);
        assert!(cursor.is_empty());
    }

    #[test]
    fn frame_prefix_is_big_endian_length() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Request::Ping).unwrap();
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(len, buf.len() - 4);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_be_bytes());
        let mut cursor = &buf[..];
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(err, CodecError::FrameTooLarge(_, _)));
    }

    #[test]
    fn error_response_shape() {
        let resp = Response::error("boom", Some("not_authorized"));
        let bytes = serde_json::to_vec(&resp).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("not_authorized"));
    }
}
