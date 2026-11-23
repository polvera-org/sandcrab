//! Length-prefixed frame codec.
//!
//! Every message on the wire is a 4-byte big-endian `u32` payload length followed by exactly
//! that many bytes of UTF-8 JSON (a JSON-RPC 2.0 object). The codec is synchronous and
//! runtime-agnostic: async transports read [`HEADER_LEN`] bytes, validate them with
//! [`parse_header`], then read the body; or feed arbitrary chunks into a [`FrameDecoder`].

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::ProtocolError;

/// Size of the big-endian `u32` length prefix.
pub const HEADER_LEN: usize = 4;

/// Largest accepted frame payload (16 MiB). Larger frames are rejected before allocation.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Errors raised while encoding, decoding, reading or writing frames.
#[derive(Debug, Error)]
pub enum FrameError {
    /// The header announced (or the encoder produced) a payload above [`MAX_FRAME_LEN`].
    #[error("frame payload of {len} bytes exceeds maximum of {max} bytes")]
    TooLarge { len: usize, max: usize },

    /// The header announced a zero-length payload, which is never valid JSON-RPC.
    #[error("zero-length frame")]
    Empty,

    /// The stream ended in the middle of a header or payload.
    #[error("stream ended mid-frame: expected {expected} bytes, got {got}")]
    Truncated { expected: usize, got: usize },

    /// A previous framing error desynchronised the stream; the decoder refuses further input.
    #[error("frame decoder is poisoned by an earlier framing error")]
    Poisoned,

    /// The payload is not valid JSON for the requested type.
    #[error("invalid frame payload: {0}")]
    Json(#[from] serde_json::Error),

    /// Underlying transport I/O failure.
    #[error("frame i/o error: {0}")]
    Io(#[from] io::Error),
}

impl From<FrameError> for ProtocolError {
    fn from(err: FrameError) -> Self {
        match err {
            FrameError::TooLarge { len, max } => ProtocolError::TooLarge {
                size: len as u64,
                limit: max as u64,
            },
            FrameError::Json(e) => ProtocolError::Parse(e.to_string()),
            FrameError::Io(e) => ProtocolError::Internal(e.to_string()),
            other => ProtocolError::InvalidRequest(other.to_string()),
        }
    }
}

/// Validate a length header and return the payload length it announces.
pub fn parse_header(header: [u8; HEADER_LEN]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    Ok(len)
}

/// Serialize `msg` as JSON and prepend the length header.
pub fn encode_frame<T: Serialize + ?Sized>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let mut buf = vec![0u8; HEADER_LEN];
    serde_json::to_writer(&mut buf, msg)?;
    let len = buf.len() - HEADER_LEN;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    // `len <= MAX_FRAME_LEN < u32::MAX`, so the cast is lossless.
    buf[..HEADER_LEN].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(buf)
}

/// Deserialize a frame payload (without its header).
pub fn decode_frame<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(payload)?)
}

/// Incremental decoder: push arbitrary byte chunks, pull complete frame payloads.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    poisoned: bool,
}

impl FrameDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes received from the transport.
    pub fn push(&mut self, chunk: &[u8]) {
        if !self.poisoned {
            self.buf.extend_from_slice(chunk);
        }
    }

    /// Pop the next complete frame payload, `Ok(None)` if more bytes are needed.
    ///
    /// An invalid header poisons the decoder: the byte stream can no longer be trusted
    /// to be aligned on frame boundaries, so the connection should be closed.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.poisoned {
            return Err(FrameError::Poisoned);
        }
        let Some(header) = self.buf.first_chunk::<HEADER_LEN>() else {
            return Ok(None);
        };
        let len = match parse_header(*header) {
            Ok(len) => len,
            Err(e) => {
                self.poisoned = true;
                self.buf = Vec::new();
                return Err(e);
            }
        };
        if self.buf.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let payload = self.buf[HEADER_LEN..HEADER_LEN + len].to_vec();
        self.buf.drain(..HEADER_LEN + len);
        Ok(Some(payload))
    }

    /// Pop and deserialize the next complete frame.
    pub fn next_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        self.next_frame()?
            .map(|payload| decode_frame(&payload))
            .transpose()
    }

    /// Number of buffered bytes not yet returned as a frame.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Call at end-of-stream: errors if a partial frame is still buffered.
    pub fn finish(&self) -> Result<(), FrameError> {
        if self.poisoned {
            return Err(FrameError::Poisoned);
        }
        match self.buf.len() {
            0 => Ok(()),
            got if got < HEADER_LEN => Err(FrameError::Truncated {
                expected: HEADER_LEN,
                got,
            }),
            got => {
                let header = [self.buf[0], self.buf[1], self.buf[2], self.buf[3]];
                let len = parse_header(header)?;
                Err(FrameError::Truncated {
                    expected: HEADER_LEN + len,
                    got,
                })
            }
        }
    }
}

/// Read until `buf` is full or EOF; returns the number of bytes read.
fn read_full<R: Read + ?Sized>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Blocking read of one frame payload. `Ok(None)` on clean EOF at a frame boundary.
pub fn read_frame<R: Read + ?Sized>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; HEADER_LEN];
    match read_full(reader, &mut header)? {
        0 => return Ok(None),
        HEADER_LEN => {}
        got => {
            return Err(FrameError::Truncated {
                expected: HEADER_LEN,
                got,
            });
        }
    }
    let len = parse_header(header)?;
    let mut payload = vec![0u8; len];
    let got = read_full(reader, &mut payload)?;
    if got < len {
        return Err(FrameError::Truncated { expected: len, got });
    }
    Ok(Some(payload))
}

/// Blocking read and deserialize of one frame. `Ok(None)` on clean EOF.
pub fn read_message<T: DeserializeOwned, R: Read + ?Sized>(
    reader: &mut R,
) -> Result<Option<T>, FrameError> {
    read_frame(reader)?
        .map(|payload| decode_frame(&payload))
        .transpose()
}

/// Blocking serialize + write of one frame (header and payload in one `write_all`).
pub fn write_frame<T: Serialize + ?Sized, W: Write + ?Sized>(
    writer: &mut W,
    msg: &T,
) -> Result<(), FrameError> {
    let frame = encode_frame(msg)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}
