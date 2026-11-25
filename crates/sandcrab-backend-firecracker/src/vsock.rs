//! Host side of Firecracker's vsock-over-UDS (design §4, upstream `docs/vsock.md`
//! "Host-Initiated Connections").
//!
//! The host connects to the VM's vsock Unix socket, writes `CONNECT <port>\n` and reads one
//! line back: `OK <host_port>\n` means the stream is now spliced to the guest listener on
//! `<port>`. If nothing listens in the guest, Firecracker closes the stream instead.

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sandcrab_core::{BoxedStream, Connector};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Longest accepted handshake reply (`OK 4294967295\n` is 14 bytes).
const MAX_REPLY: usize = 32;

/// Connect to guest vsock `port` through the Firecracker vsock UDS at `uds`.
///
/// Any reply other than `OK <u32>\n` (including EOF, which Firecracker sends when no guest
/// listener exists yet) is [`io::ErrorKind::ConnectionRefused`], so callers can retry.
pub async fn vsock_connect(uds: &Path, port: u32) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(uds).await?;
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    // Read byte by byte so nothing after the handshake line is consumed.
    let mut line = Vec::with_capacity(16);
    let mut b = [0u8; 1];
    loop {
        let n = match stream.read(&mut b).await {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => 0,
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Err(refused(port, "closed by the VMM (no guest listener yet?)"));
        }
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
        if line.len() >= MAX_REPLY {
            return Err(refused(port, "handshake reply too long"));
        }
    }
    parse_ok(&line).ok_or_else(|| {
        refused(
            port,
            &format!(
                "unexpected handshake reply `{}`",
                String::from_utf8_lossy(&line)
            ),
        )
    })?;
    Ok(stream)
}

fn refused(port: u32, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionRefused,
        format!("vsock CONNECT {port}: {why}"),
    )
}

/// Parse `OK <u32>` (without the newline) → host-side port.
fn parse_ok(line: &[u8]) -> Option<u32> {
    std::str::from_utf8(line)
        .ok()?
        .strip_prefix("OK ")?
        .parse()
        .ok()
}

/// A [`Connector`] that opens a fresh vsock stream to one guest port per call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VsockConnector {
    pub uds: PathBuf,
    pub port: u32,
}

impl VsockConnector {
    #[must_use]
    pub fn new(uds: impl Into<PathBuf>, port: u32) -> Self {
        Self {
            uds: uds.into(),
            port,
        }
    }
}

#[async_trait]
impl Connector for VsockConnector {
    async fn connect(&self) -> io::Result<BoxedStream> {
        Ok(Box::new(vsock_connect(&self.uds, self.port).await?))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sandcrab_core::testing::MockCrabd;
    use sandcrab_core::wait_ready;
    use tokio::net::UnixListener;

    use super::*;

    #[test]
    fn parses_ok_lines() {
        assert_eq!(parse_ok(b"OK 1073741824"), Some(1_073_741_824));
        assert_eq!(parse_ok(b"OK"), None);
        assert_eq!(parse_ok(b"ok 1"), None);
        assert_eq!(parse_ok(b"OK -1"), None);
        assert_eq!(parse_ok(b"OK 1 2"), None);
    }

    /// Fake Firecracker vsock UDS: port 1024 → mock crabd; port 7 → garbage reply; any other
    /// port → immediate close (no guest listener).
    async fn fake_vsock(listener: UnixListener) {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut line = Vec::new();
                let mut b = [0u8; 1];
                while s.read(&mut b).await.unwrap_or(0) == 1 && b[0] != b'\n' {
                    line.push(b[0]);
                }
                match String::from_utf8(line).unwrap().as_str() {
                    "CONNECT 1024" => {
                        // Reply and the first bytes of the stream may share one write.
                        s.write_all(b"OK 1073741824\n").await.unwrap();
                        MockCrabd::default().serve(s).await;
                    }
                    "CONNECT 7" => s.write_all(b"NOPE\n").await.unwrap(),
                    _ => {}
                }
            });
        }
    }

    #[tokio::test]
    async fn handshake_and_crabd_over_fake_vsock() {
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("v.sock");
        tokio::spawn(fake_vsock(UnixListener::bind(&uds).unwrap()));

        let ready = wait_ready(
            &VsockConnector::new(&uds, 1024),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(ready.attempts, 1);
        ready.client.close().await;

        let err = vsock_connect(&uds, 5).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
        let err = vsock_connect(&uds, 7).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert!(err.to_string().contains("NOPE"), "{err}");
        let err = vsock_connect(&dir.path().join("missing.sock"), 1)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
