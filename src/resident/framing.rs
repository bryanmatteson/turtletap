use std::io;

use serde::{Serialize, de::DeserializeOwned};

/// Largest accepted wire frame.
pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

/// Framing and payload errors.
#[derive(Debug)]
pub enum FrameError {
    /// Underlying transport failed.
    Io(io::Error),
    /// A peer declared an excessive payload.
    TooLarge(usize),
    /// JSON encoding or decoding failed.
    Json(serde_json::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::TooLarge(size) => write!(
                formatter,
                "resident frame is {size} bytes; maximum is {MAX_FRAME_SIZE} bytes"
            ),
            Self::Json(error) => write!(formatter, "invalid resident JSON: {error}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for FrameError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Encodes one bounded JSON value as a four-byte big-endian length-prefixed frame.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge(payload.len()))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Incrementally decodes bounded frames from arbitrary transport chunks.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    bytes: Vec<u8>,
}

impl FrameDecoder {
    /// Adds bytes received from a transport.
    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Removes the next complete frame's payload, leaving partial data buffered.
    ///
    /// Because all decoding state lives in the decoder rather than in a
    /// suspended future, a reader built on this method stays correct when its
    /// read is cancelled mid-frame.
    pub fn next_payload(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.bytes.len() < 4 {
            return Ok(None);
        }
        let size = u32::from_be_bytes([self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]])
            as usize;
        if size > MAX_FRAME_SIZE {
            return Err(FrameError::TooLarge(size));
        }
        if self.bytes.len() < 4 + size {
            return Ok(None);
        }
        let payload = self.bytes[4..4 + size].to_vec();
        self.bytes.drain(..4 + size);
        Ok(Some(payload))
    }

    /// Decodes the next complete value, leaving partial data buffered.
    pub fn decode_next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        match self.next_payload()? {
            Some(payload) => Ok(Some(serde_json::from_slice(&payload)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_retains_partial_frames() {
        let frame = encode_frame(&vec!["one", "two"]).expect("encode");
        let mut decoder = FrameDecoder::default();
        decoder.push(&frame[..3]);
        assert!(
            decoder
                .decode_next::<Vec<String>>()
                .expect("decode")
                .is_none()
        );
        decoder.push(&frame[3..]);
        assert_eq!(
            decoder.decode_next::<Vec<String>>().expect("decode"),
            Some(vec!["one".to_owned(), "two".to_owned()])
        );
    }

    #[test]
    fn decoder_rejects_oversized_declarations_before_allocating() {
        let mut decoder = FrameDecoder::default();
        decoder.push(
            &u32::try_from(MAX_FRAME_SIZE + 1)
                .expect("limit fits")
                .to_be_bytes(),
        );
        assert!(matches!(
            decoder.decode_next::<serde_json::Value>(),
            Err(FrameError::TooLarge(_))
        ));
    }
}
