//! Async frame I/O over any `AsyncRead` / `AsyncWrite`.
//!
//! Thin adapter over the runtime-agnostic helpers in [`sandcrab_protocol`]: read the
//! [`HEADER_LEN`]-byte big-endian length, validate it with [`parse_header`] (rejects empty and
//! `> MAX_FRAME_LEN` frames *before* allocating), then read exactly that many payload bytes.
//!
//! [`read_frame`] is **not** cancel-safe: dropping it mid-frame loses the bytes read so far.
//! The server only cancels it when it is about to close the connection anyway.

use std::io;

use sandcrab_protocol::{FrameError, HEADER_LEN, encode_frame, parse_header};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one frame payload. `Ok(None)` on clean EOF at a frame boundary.
///
/// Errors: [`FrameError::Empty`] / [`FrameError::TooLarge`] for a bad header (the stream is
/// then desynchronised and must be closed), [`FrameError::Truncated`] on EOF mid-frame,
/// [`FrameError::Io`] on transport failure.
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; HEADER_LEN];
    let got = read_full(reader, &mut header).await?;
    match got {
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
    let got = read_full(reader, &mut payload).await?;
    if got < len {
        return Err(FrameError::Truncated { expected: len, got });
    }
    Ok(Some(payload))
}

/// Serialize `msg`, frame it and write it in one `write_all`, then flush.
pub async fn write_frame<T: Serialize + ?Sized, W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    msg: &T,
) -> Result<(), FrameError> {
    let frame = encode_frame(msg)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Read until `buf` is full or EOF; returns the number of bytes read.
async fn read_full<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    buf: &mut [u8],
) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]).await {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandcrab_protocol::{MAX_FRAME_LEN, RpcRequest};

    #[tokio::test]
    async fn round_trip_and_clean_eof() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let req = RpcRequest::new(7, "ping", None);
        write_frame(&mut a, &req).await.unwrap();
        drop(a);
        let payload = read_frame(&mut b).await.unwrap().unwrap();
        let back: RpcRequest = serde_json::from_slice(&payload).unwrap();
        assert_eq!(back, req);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_oversize_header_without_reading_body() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let len = u32::try_from(MAX_FRAME_LEN + 1).unwrap();
        a.write_all(&len.to_be_bytes()).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn truncated_header_and_body() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0, 0]).await.unwrap();
        drop(a);
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(
            err,
            FrameError::Truncated {
                expected: 4,
                got: 2
            }
        ));

        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&10u32.to_be_bytes()).await.unwrap();
        a.write_all(b"{}").await.unwrap();
        drop(a);
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(
            err,
            FrameError::Truncated {
                expected: 10,
                got: 2
            }
        ));
    }

    #[tokio::test]
    async fn rejects_empty_frame() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&0u32.to_be_bytes()).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::Empty));
    }
}
