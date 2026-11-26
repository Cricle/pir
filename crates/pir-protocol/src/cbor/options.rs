use thiserror::Error;

pub const DEFAULT_MAX_CBOR_BYTE_LENGTH: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
pub const DEFAULT_MAX_CBOR_DEPTH: usize = 64;
const MAX_CONFIGURED_DEPTH: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CborOptions {
    pub max_byte_length: usize,
    pub max_container_length: usize,
    pub max_depth: usize,
}

impl Default for CborOptions {
    fn default() -> Self {
        Self {
            max_byte_length: DEFAULT_MAX_CBOR_BYTE_LENGTH,
            max_container_length: DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
            max_depth: DEFAULT_MAX_CBOR_DEPTH,
        }
    }
}

impl CborOptions {
    pub fn validate(self) -> Result<Self, CborError> {
        if self.max_byte_length > u32::MAX as usize {
            return Err(CborError::InvalidLimit("max_byte_length"));
        }
        if self.max_container_length > u32::MAX as usize {
            return Err(CborError::InvalidLimit("max_container_length"));
        }
        if self.max_depth > MAX_CONFIGURED_DEPTH {
            return Err(CborError::InvalidLimit("max_depth"));
        }
        Ok(self)
    }
}

#[derive(Debug, Error)]
pub enum CborError {
    #[error("invalid CBOR limit: {0}")]
    InvalidLimit(&'static str),
    #[error("CBOR serialization failed: {0}")]
    Serialization(String),
    #[error("CBOR byte length exceeds configured limit of {0}")]
    ByteLimit(usize),
    #[error("CBOR container length exceeds configured limit of {0}")]
    ContainerLimit(usize),
    #[error("CBOR nesting depth exceeds configured limit of {0}")]
    DepthLimit(usize),
    #[error("malformed CBOR: {0}")]
    Malformed(String),
}
