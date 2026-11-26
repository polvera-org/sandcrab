//! Async frames on a server connection: the `crabd` codec (u32 big-endian length, one JSON-RPC
//! object, at most 16 MiB), read with Tokio.

use sandcrab_sdk::protocol::{
    FrameError, HEADER_LEN, ProtocolError, RpcResponse, encode_frame, parse_header,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one frame payload. `Ok(None)`: the peer closed between frames. A header over the cap
/// or of length zero is an error before anything is allocated; the stream is then out of
/// step and must be closed.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        let n = r.read(&mut header[got..]).await?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(FrameError::Truncated {
                    expected: HEADER_LEN,
                    got,
                })
            };
        }
        got += n;
    }
    let len = parse_header(header)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::Truncated {
                expected: len,
                got: 0,
            }
        } else {
            FrameError::Io(e)
        }
    })?;
    Ok(Some(payload))
}

/// Encode `msg` as one frame.
#[must_use]
pub fn frame<T: serde::Serialize + ?Sized>(msg: &T) -> Vec<u8> {
    encode_frame(msg).unwrap_or_else(|e| {
        // Only an answer above 16 MiB fails to encode; send an error instead.
        let err = ProtocolError::Internal(format!("answer not sent: {e}"));
        encode_frame(&RpcResponse::failure(None, &err)).expect("a short error encodes")
    })
}

/// Write one already encoded frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(bytes).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_frames_eof_and_rejects_bad_headers() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &frame(&serde_json::json!({"x": 1})))
            .await
            .unwrap();
        drop(a);
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), br#"{"x":1}"#);
        assert!(read_frame(&mut b).await.unwrap().is_none());

        for header in [[0u8; 4], (17u32 * 1024 * 1024).to_be_bytes()] {
            let (mut a, mut b) = tokio::io::duplex(64);
            a.write_all(&header).await.unwrap();
            assert!(read_frame(&mut b).await.is_err());
        }

        // Cut in the header, and in the payload.
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0, 0]).await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame(&mut b).await,
            Err(FrameError::Truncated { .. })
        ));
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0, 0, 0, 9, b'{']).await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame(&mut b).await,
            Err(FrameError::Truncated { .. })
        ));
    }
}
