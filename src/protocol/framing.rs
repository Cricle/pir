//! Four-byte big-endian length-prefixed framing.

use thiserror::Error;

pub const FRAME_HEADER_LENGTH: usize = 4;
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;
const MAX_PAYLOAD_BLOCK: usize = 64 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame payload exceeds the unsigned 32-bit length limit")]
    LengthOverflow,
    #[error("frame length {length} exceeds configured limit of {limit}")]
    Limit { length: usize, limit: usize },
    #[error("frame does not contain a complete length prefix")]
    IncompleteHeader,
    #[error("frame must contain exactly one complete payload")]
    IncompletePayload,
    #[error("frame decoder has ended")]
    Ended,
    #[error("frame decoder has failed")]
    Failed,
    #[error("truncated frame at end of stream")]
    Truncated,
    #[error("invalid frame limit")]
    InvalidLimit,
}

#[derive(Clone, Copy, Debug)]
pub struct FrameOptions {
    pub max_frame_length: usize,
}

impl Default for FrameOptions {
    fn default() -> Self {
        Self {
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
        }
    }
}

impl FrameOptions {
    fn validate(self) -> Result<Self, FrameError> {
        if self.max_frame_length > u32::MAX as usize {
            Err(FrameError::InvalidLimit)
        } else {
            Ok(self)
        }
    }
}

/// Prefix a payload with its unsigned 32-bit big-endian byte length.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > u32::MAX as usize {
        return Err(FrameError::LengthOverflow);
    }
    let length = payload.len() as u32;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LENGTH + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Single-allocation frame builder for callers that already produce their
/// payload in a `Vec` and need to hand the resulting frame to a queue.
///
/// [`encode_frame`] allocates the frame and copies the payload into it. This
/// builder reserves the four header bytes up front, so writing the payload
/// through [`payload_mut`](Self::payload_mut) and calling
/// [`finish`](Self::finish) yields the identical wire bytes without a second
/// allocation or a payload copy.
pub struct FrameBuilder {
    bytes: Vec<u8>,
}

impl FrameBuilder {
    /// A builder whose payload can grow to `payload_capacity` bytes without
    /// reallocating (the four header bytes are already reserved).
    pub fn with_capacity(payload_capacity: usize) -> Self {
        let mut bytes = Vec::with_capacity(FRAME_HEADER_LENGTH + payload_capacity);
        bytes.extend_from_slice(&[0; FRAME_HEADER_LENGTH]);
        Self { bytes }
    }

    /// The payload buffer following the reserved header. Callers must only
    /// append payload bytes; the header is filled in by
    /// [`finish`](Self::finish).
    pub fn payload_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// Length of the payload written so far.
    pub fn payload_len(&self) -> usize {
        self.bytes.len().saturating_sub(FRAME_HEADER_LENGTH)
    }

    /// Fill in the big-endian length header and return the complete frame.
    pub fn finish(mut self) -> Result<Vec<u8>, FrameError> {
        if self.bytes.len() < FRAME_HEADER_LENGTH {
            return Err(FrameError::LengthOverflow);
        }
        let payload_len = self.payload_len();
        if payload_len > u32::MAX as usize {
            return Err(FrameError::LengthOverflow);
        }
        self.bytes[..FRAME_HEADER_LENGTH].copy_from_slice(&(payload_len as u32).to_be_bytes());
        Ok(self.bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Open,
    Ended,
    Failed,
}

/// Incrementally splits arbitrary byte chunks into length-prefixed payloads.
pub struct FrameDecoder {
    header: [u8; FRAME_HEADER_LENGTH],
    header_len: usize,
    max_frame_length: usize,
    expected_length: Option<usize>,
    payload: Vec<u8>,
    state: State,
}

impl FrameDecoder {
    pub fn new(options: FrameOptions) -> Result<Self, FrameError> {
        let options = options.validate()?;
        Ok(Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_len: 0,
            max_frame_length: options.max_frame_length,
            expected_length: None,
            payload: Vec::new(),
            state: State::Open,
        })
    }

    pub fn with_default_limits() -> Self {
        Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_len: 0,
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
            expected_length: None,
            payload: Vec::new(),
            state: State::Open,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        match self.state {
            State::Open => {}
            State::Ended => return Err(FrameError::Ended),
            State::Failed => return Err(FrameError::Failed),
        }
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
            if self.expected_length.is_none() {
                let amount = (FRAME_HEADER_LENGTH - self.header_len).min(chunk.len() - offset);
                self.header[self.header_len..self.header_len + amount]
                    .copy_from_slice(&chunk[offset..offset + amount]);
                self.header_len += amount;
                offset += amount;
                if self.header_len < FRAME_HEADER_LENGTH {
                    continue;
                }
                let length = u32::from_be_bytes(self.header) as usize;
                self.header_len = 0;
                if length > self.max_frame_length {
                    return self.fail(FrameError::Limit {
                        length,
                        limit: self.max_frame_length,
                    });
                }
                if length == 0 {
                    frames.push(Vec::new());
                    continue;
                }
                self.expected_length = Some(length);
                self.payload = Vec::with_capacity(length.min(MAX_PAYLOAD_BLOCK));
            }
            let expected = match self.expected_length {
                Some(value) => value,
                None => continue,
            };
            let remaining = expected - self.payload.len();
            let amount = remaining.min(chunk.len() - offset);
            self.payload
                .extend_from_slice(&chunk[offset..offset + amount]);
            offset += amount;
            if self.payload.len() == expected {
                let payload = std::mem::take(&mut self.payload);
                self.expected_length = None;
                frames.push(payload);
            }
        }
        Ok(frames)
    }

    pub fn end(&mut self) -> Result<(), FrameError> {
        match self.state {
            State::Ended => return Err(FrameError::Ended),
            State::Failed => return Err(FrameError::Failed),
            State::Open => {}
        }
        if self.header_len != 0 || self.expected_length.is_some() {
            return self.fail(FrameError::Truncated);
        }
        self.state = State::Ended;
        Ok(())
    }

    fn fail<T>(&mut self, error: FrameError) -> Result<T, FrameError> {
        self.state = State::Failed;
        self.header_len = 0;
        self.expected_length = None;
        self.payload.clear();
        Err(error)
    }
}
