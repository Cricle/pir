//! Length-prefixed frames: `u32 length | u8 type | u32 id | u32 jsonLength | json | payload` (docs/protocol.md).

use serde_json::Value;
use std::io::{self, Read, Write};

pub const REQUEST: u8 = 1;
pub const RESULT: u8 = 2;
pub const ERROR: u8 = 3;
pub const EVENT: u8 = 4;
pub const CANCEL: u8 = 5;
pub const PING: u8 = 6;

/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Largest payload a response carries, leaving room for its JSON.
pub const MAX_PAYLOAD: usize = MAX_FRAME - 64 * 1024;

pub struct Frame {
    pub kind: u8,
    pub id: u32,
    pub json: Value,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: u8, id: u32, json: Value) -> Frame {
        Frame {
            kind,
            id,
            json,
            payload: Vec::new(),
        }
    }

    pub fn with_payload(kind: u8, id: u32, json: Value, payload: Vec<u8>) -> Frame {
        Frame {
            kind,
            id,
            json,
            payload,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// Read one frame; `None` at a clean end of input.
pub fn read_frame(input: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut length = [0u8; 4];
    match input.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes(length) as usize;
    if !(9..=MAX_FRAME).contains(&length) {
        return Err(invalid("frame length out of range"));
    }
    // Read the 9 fixed body bytes into a stack array, then the JSON text and
    // the payload straight into their own buffers. The previous shape
    // allocated the whole body (JSON + payload), copied the payload out of
    // it, and dropped the body: one full-payload copy per frame. The wire
    // format is unchanged.
    let mut header = [0u8; 9];
    input.read_exact(&mut header)?;
    let kind = header[0];
    let id = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let json_length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    // Checked arithmetic: `json_length` is wire-controlled and `9 + …` would
    // wrap on a 32-bit target, turning the bound into a huge allocation.
    let body_after_header = length
        .checked_sub(9)
        .ok_or_else(|| invalid("frame length out of range"))?;
    if json_length > body_after_header {
        return Err(invalid("JSON length out of range"));
    }
    let payload_length = body_after_header - json_length;
    // Unparseable JSON leaves the framing intact: the frame arrives with `Value::Null` and gets an error reply.
    let json = if json_length == 0 {
        Value::Object(Default::default())
    } else {
        let mut raw = vec![0u8; json_length];
        input.read_exact(&mut raw)?;
        serde_json::from_slice(&raw).unwrap_or(Value::Null)
    };
    // The only allocation the payload pays: its own final buffer, filled
    // directly from the input (never a body allocation plus a copy).
    let mut payload = vec![0u8; payload_length];
    if payload_length != 0 {
        input.read_exact(&mut payload)?;
    }
    Ok(Some(Frame {
        kind,
        id,
        json,
        payload,
    }))
}

pub fn write_frame(output: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let json = serde_json::to_vec(&frame.json).map_err(|_| invalid("unserializable JSON"))?;
    let length = 9usize
        .checked_add(json.len())
        .and_then(|length| length.checked_add(frame.payload.len()))
        .ok_or_else(|| invalid("frame too large"))?;
    if length > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(length as u32).to_be_bytes());
    header.push(frame.kind);
    header.extend_from_slice(&frame.id.to_be_bytes());
    header.extend_from_slice(&(json.len() as u32).to_be_bytes());
    output.write_all(&header)?;
    output.write_all(&json)?;
    output.write_all(&frame.payload)?;
    output.flush()
}
