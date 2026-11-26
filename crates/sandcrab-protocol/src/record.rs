//! Record codec for process and forward connections.
//!
//! After the ack of `process.open` or `forward.accept` a connection leaves frame mode and
//! carries records:
//!
//! ```text
//! +---------+----------------------+------------------+
//! | u8 kind | u32 length, big-end. | `length` bytes   |
//! +---------+----------------------+------------------+
//! ```
//!
//! | Kind | Connection, direction | Payload | Max length |
//! | --- | --- | --- | --- |
//! | `0x01 STDIN` | process, towards the guest | bytes for the program's stdin | 64 KiB |
//! | `0x02 STDIN_EOF` | process, towards the guest | empty; closes the program's stdin | 0 |
//! | `0x11 STDOUT` | process, from the guest | stdout bytes (and stderr with `merge`) | 64 KiB |
//! | `0x12 STDERR` | process, from the guest | stderr bytes, only with `stderr: "stream"` | 64 KiB |
//! | `0x1F EXIT` | process, from the guest | JSON [`crate::ProcessExit`]; always the last record | 256 KiB |
//! | `0x21 DATA` | forward, both ways | bytes | 64 KiB |
//! | `0x22 EOF` | forward, both ways | empty; this side sends no more | 0 |
//!
//! A kind that is unknown or not valid on the connection and direction ([`RecordChannel`]),
//! or a length above the kind's maximum, is a fatal error: the receiver closes the
//! connection. Payloads are bytes; nothing here treats them as text.
//!
//! The codec is pure and synchronous, like [`crate::FrameDecoder`]: async transports read
//! [`RECORD_HEADER_LEN`] bytes, validate them with [`parse_record_header`], then read the
//! payload; or feed arbitrary chunks into a [`RecordDecoder`].

use std::fmt;

use thiserror::Error;

/// Size of a record header: one kind byte and a big-endian `u32` length.
pub const RECORD_HEADER_LEN: usize = 5;
/// Largest payload of a data record (`STDIN`, `STDOUT`, `STDERR`, `DATA`).
pub const MAX_RECORD_DATA_LEN: usize = 64 * 1024;
/// Largest payload of an `EXIT` record.
pub const MAX_EXIT_RECORD_LEN: usize = 256 * 1024;

/// The kind byte of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    Stdin = 0x01,
    StdinEof = 0x02,
    Stdout = 0x11,
    Stderr = 0x12,
    Exit = 0x1F,
    Data = 0x21,
    Eof = 0x22,
}

impl RecordKind {
    /// The kind for a wire byte, `None` if unknown.
    #[must_use]
    pub fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0x01 => Self::Stdin,
            0x02 => Self::StdinEof,
            0x11 => Self::Stdout,
            0x12 => Self::Stderr,
            0x1F => Self::Exit,
            0x21 => Self::Data,
            0x22 => Self::Eof,
            _ => return None,
        })
    }

    /// The wire byte.
    #[must_use]
    pub fn byte(self) -> u8 {
        self as u8
    }

    /// Largest payload this kind may carry.
    #[must_use]
    pub fn max_len(self) -> usize {
        match self {
            Self::StdinEof | Self::Eof => 0,
            Self::Exit => MAX_EXIT_RECORD_LEN,
            Self::Stdin | Self::Stdout | Self::Stderr | Self::Data => MAX_RECORD_DATA_LEN,
        }
    }

    /// Whether this kind may be received on `channel`.
    #[must_use]
    pub fn allowed_on(self, channel: RecordChannel) -> bool {
        match channel {
            RecordChannel::ProcessInput => matches!(self, Self::Stdin | Self::StdinEof),
            RecordChannel::ProcessOutput => {
                matches!(self, Self::Stdout | Self::Stderr | Self::Exit)
            }
            RecordChannel::Forward => matches!(self, Self::Data | Self::Eof),
        }
    }
}

impl fmt::Display for RecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdin => "STDIN",
            Self::StdinEof => "STDIN_EOF",
            Self::Stdout => "STDOUT",
            Self::Stderr => "STDERR",
            Self::Exit => "EXIT",
            Self::Data => "DATA",
            Self::Eof => "EOF",
        })
    }
}

/// The stream a receiver reads records from. Each accepts only its own kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordChannel {
    /// A process connection, towards the guest: `STDIN`, `STDIN_EOF`.
    ProcessInput,
    /// A process connection, from the guest: `STDOUT`, `STDERR`, `EXIT`.
    ProcessOutput,
    /// A forward connection, either direction: `DATA`, `EOF`.
    Forward,
}

/// One decoded record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

/// Errors of the record codec. Every decode error means the stream can no longer be trusted:
/// close the connection.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RecordError {
    #[error("unknown record kind 0x{0:02x}")]
    UnknownKind(u8),

    #[error("record kind {kind} is not valid on a {channel:?} stream")]
    UnexpectedKind {
        kind: RecordKind,
        channel: RecordChannel,
    },

    #[error("{kind} record of {len} bytes exceeds maximum of {max} bytes")]
    TooLong {
        kind: RecordKind,
        len: usize,
        max: usize,
    },

    #[error("stream ended mid-record: expected {expected} bytes, got {got}")]
    Truncated { expected: usize, got: usize },

    #[error("record decoder is poisoned by an earlier error")]
    Poisoned,
}

/// The header of a `kind` record with a `len`-byte payload. Fails if `len` is over the kind's
/// maximum.
pub fn encode_record_header(
    kind: RecordKind,
    len: usize,
) -> Result<[u8; RECORD_HEADER_LEN], RecordError> {
    let max = kind.max_len();
    if len > max {
        return Err(RecordError::TooLong { kind, len, max });
    }
    let mut h = [0u8; RECORD_HEADER_LEN];
    h[0] = kind.byte();
    // `len <= MAX_EXIT_RECORD_LEN < u32::MAX`, so the cast is lossless.
    h[1..].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(h)
}

/// A complete record: header and payload.
pub fn encode_record(kind: RecordKind, payload: &[u8]) -> Result<Vec<u8>, RecordError> {
    let header = encode_record_header(kind, payload.len())?;
    let mut buf = Vec::with_capacity(RECORD_HEADER_LEN + payload.len());
    buf.extend_from_slice(&header);
    buf.extend_from_slice(payload);
    Ok(buf)
}

/// Validate a record header read from `channel` and return its kind and payload length.
/// Oversized lengths are rejected before any allocation.
pub fn parse_record_header(
    channel: RecordChannel,
    header: [u8; RECORD_HEADER_LEN],
) -> Result<(RecordKind, usize), RecordError> {
    let kind = RecordKind::from_byte(header[0]).ok_or(RecordError::UnknownKind(header[0]))?;
    if !kind.allowed_on(channel) {
        return Err(RecordError::UnexpectedKind { kind, channel });
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let max = kind.max_len();
    if len > max {
        return Err(RecordError::TooLong { kind, len, max });
    }
    Ok((kind, len))
}

/// Incremental decoder: push arbitrary byte chunks, pull complete records.
#[derive(Debug)]
pub struct RecordDecoder {
    channel: RecordChannel,
    buf: Vec<u8>,
    poisoned: bool,
}

impl RecordDecoder {
    /// A decoder that accepts only the kinds of `channel`.
    #[must_use]
    pub fn new(channel: RecordChannel) -> Self {
        Self {
            channel,
            buf: Vec::new(),
            poisoned: false,
        }
    }

    /// Append bytes received from the transport.
    pub fn push(&mut self, chunk: &[u8]) {
        if !self.poisoned {
            self.buf.extend_from_slice(chunk);
        }
    }

    /// Pop the next complete record, `Ok(None)` if more bytes are needed.
    ///
    /// An invalid header poisons the decoder: close the connection.
    pub fn next_record(&mut self) -> Result<Option<Record>, RecordError> {
        if self.poisoned {
            return Err(RecordError::Poisoned);
        }
        let Some(header) = self.buf.first_chunk::<RECORD_HEADER_LEN>() else {
            return Ok(None);
        };
        let (kind, len) = match parse_record_header(self.channel, *header) {
            Ok(v) => v,
            Err(e) => {
                self.poisoned = true;
                self.buf = Vec::new();
                return Err(e);
            }
        };
        if self.buf.len() < RECORD_HEADER_LEN + len {
            return Ok(None);
        }
        let payload = self.buf[RECORD_HEADER_LEN..RECORD_HEADER_LEN + len].to_vec();
        self.buf.drain(..RECORD_HEADER_LEN + len);
        Ok(Some(Record { kind, payload }))
    }

    /// Number of buffered bytes not yet returned as a record.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Call at end-of-stream: errors if a partial record is still buffered.
    pub fn finish(&self) -> Result<(), RecordError> {
        if self.poisoned {
            return Err(RecordError::Poisoned);
        }
        match self.buf.len() {
            0 => Ok(()),
            got if got < RECORD_HEADER_LEN => Err(RecordError::Truncated {
                expected: RECORD_HEADER_LEN,
                got,
            }),
            got => {
                let header = *self
                    .buf
                    .first_chunk::<RECORD_HEADER_LEN>()
                    .expect("len checked");
                let (_, len) = parse_record_header(self.channel, header)?;
                Err(RecordError::Truncated {
                    expected: RECORD_HEADER_LEN + len,
                    got,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [RecordKind; 7] = [
        RecordKind::Stdin,
        RecordKind::StdinEof,
        RecordKind::Stdout,
        RecordKind::Stderr,
        RecordKind::Exit,
        RecordKind::Data,
        RecordKind::Eof,
    ];

    fn channel_of(kind: RecordKind) -> RecordChannel {
        [
            RecordChannel::ProcessInput,
            RecordChannel::ProcessOutput,
            RecordChannel::Forward,
        ]
        .into_iter()
        .find(|c| kind.allowed_on(*c))
        .unwrap()
    }

    #[test]
    fn wire_bytes_and_limits() {
        let bytes: Vec<u8> = ALL.iter().map(|k| k.byte()).collect();
        assert_eq!(bytes, [0x01, 0x02, 0x11, 0x12, 0x1F, 0x21, 0x22]);
        for k in ALL {
            assert_eq!(RecordKind::from_byte(k.byte()), Some(k));
            // Each kind belongs to exactly one channel.
            let n = [
                RecordChannel::ProcessInput,
                RecordChannel::ProcessOutput,
                RecordChannel::Forward,
            ]
            .iter()
            .filter(|c| k.allowed_on(**c))
            .count();
            assert_eq!(n, 1, "{k}");
        }
        for b in (0..=255u8).filter(|b| !bytes.contains(b)) {
            assert_eq!(RecordKind::from_byte(b), None);
        }
        assert_eq!(
            encode_record(RecordKind::Stdout, b"hi").unwrap(),
            [0x11, 0, 0, 0, 2, b'h', b'i']
        );
        assert_eq!(
            encode_record(RecordKind::StdinEof, b"").unwrap(),
            [0x02, 0, 0, 0, 0]
        );
    }

    #[test]
    fn round_trip_every_kind_at_its_maximum() {
        for k in ALL {
            for len in [0, k.max_len()] {
                let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                let rec = encode_record(k, &payload).unwrap();
                let mut dec = RecordDecoder::new(channel_of(k));
                dec.push(&rec);
                assert_eq!(
                    dec.next_record().unwrap(),
                    Some(Record {
                        kind: k,
                        payload: payload.clone()
                    })
                );
                assert_eq!(dec.next_record().unwrap(), None);
                dec.finish().unwrap();
            }
        }
    }

    #[test]
    fn encoder_rejects_oversized_payloads() {
        for k in ALL {
            let len = k.max_len() + 1;
            assert_eq!(
                encode_record(k, &vec![0; len]),
                Err(RecordError::TooLong {
                    kind: k,
                    len,
                    max: k.max_len()
                })
            );
        }
    }

    #[test]
    fn split_input_byte_by_byte() {
        let mut stream = Vec::new();
        let records = [
            (RecordKind::Stdout, b"hello".to_vec()),
            (RecordKind::Stderr, vec![0xff, 0x00, 0x80]),
            (RecordKind::Stdout, Vec::new()),
            (RecordKind::Exit, br#"{"code":0}"#.to_vec()),
        ];
        for (k, p) in &records {
            stream.extend(encode_record(*k, p).unwrap());
        }
        let mut dec = RecordDecoder::new(RecordChannel::ProcessOutput);
        let mut got = Vec::new();
        for b in &stream {
            dec.push(std::slice::from_ref(b));
            while let Some(r) = dec.next_record().unwrap() {
                got.push((r.kind, r.payload));
            }
        }
        assert_eq!(got, records);
        assert_eq!(dec.buffered_len(), 0);
        dec.finish().unwrap();

        // Several records in one chunk.
        let mut dec = RecordDecoder::new(RecordChannel::ProcessOutput);
        dec.push(&stream);
        let mut n = 0;
        while dec.next_record().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, records.len());
    }

    #[test]
    fn oversized_length_poisons_before_payload_arrives() {
        for (k, ch) in [
            (RecordKind::Stdin, RecordChannel::ProcessInput),
            (RecordKind::StdinEof, RecordChannel::ProcessInput),
            (RecordKind::Exit, RecordChannel::ProcessOutput),
            (RecordKind::Eof, RecordChannel::Forward),
        ] {
            let len = k.max_len() as u32 + 1;
            let mut h = vec![k.byte()];
            h.extend(len.to_be_bytes());
            let mut dec = RecordDecoder::new(ch);
            dec.push(&h);
            assert_eq!(
                dec.next_record(),
                Err(RecordError::TooLong {
                    kind: k,
                    len: len as usize,
                    max: k.max_len()
                })
            );
            assert_eq!(dec.next_record(), Err(RecordError::Poisoned));
            dec.push(b"more");
            assert_eq!(dec.buffered_len(), 0);
            assert_eq!(dec.finish(), Err(RecordError::Poisoned));
        }
        // u32::MAX is rejected from the header alone.
        assert!(matches!(
            parse_record_header(RecordChannel::Forward, [0x21, 0xff, 0xff, 0xff, 0xff]),
            Err(RecordError::TooLong { .. })
        ));
    }

    #[test]
    fn unknown_and_misplaced_kinds_are_fatal() {
        let mut dec = RecordDecoder::new(RecordChannel::Forward);
        dec.push(&[0x7f, 0, 0, 0, 0]);
        assert_eq!(dec.next_record(), Err(RecordError::UnknownKind(0x7f)));
        assert_eq!(dec.next_record(), Err(RecordError::Poisoned));

        // Kinds of another channel or direction.
        for (k, ch) in [
            (RecordKind::Stdout, RecordChannel::ProcessInput),
            (RecordKind::Stdin, RecordChannel::ProcessOutput),
            (RecordKind::Exit, RecordChannel::ProcessInput),
            (RecordKind::Data, RecordChannel::ProcessOutput),
            (RecordKind::Stdin, RecordChannel::Forward),
        ] {
            let mut dec = RecordDecoder::new(ch);
            dec.push(&encode_record(k, b"").unwrap());
            assert_eq!(
                dec.next_record(),
                Err(RecordError::UnexpectedKind {
                    kind: k,
                    channel: ch
                })
            );
        }
    }

    #[test]
    fn truncated_stream_is_reported() {
        let mut dec = RecordDecoder::new(RecordChannel::Forward);
        dec.push(&[0x21, 0, 0]);
        assert_eq!(dec.next_record(), Ok(None));
        assert_eq!(
            dec.finish(),
            Err(RecordError::Truncated {
                expected: RECORD_HEADER_LEN,
                got: 3
            })
        );
        dec.push(&[0, 4, b'a', b'b']);
        assert_eq!(dec.next_record(), Ok(None));
        assert_eq!(
            dec.finish(),
            Err(RecordError::Truncated {
                expected: RECORD_HEADER_LEN + 4,
                got: 7
            })
        );
        dec.push(b"cd");
        assert_eq!(
            dec.next_record().unwrap().unwrap().payload,
            b"abcd".to_vec()
        );
        dec.finish().unwrap();
    }
}
