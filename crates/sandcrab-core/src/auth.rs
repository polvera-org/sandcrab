//! Host side of the `auth` frame ([`sandcrab_protocol::auth`]).
//!
//! [`AuthConnector`] wraps any [`Connector`] and authenticates every stream it opens before
//! handing it out, so the multiplexed client, readiness retries, reconnects and every
//! `tunnel.open` connection are all authenticated by construction. The token only lives in
//! memory here (and in the backend's token file until `crabd` consumes it); its `Debug` is
//! redacted, so connector errors and logs never carry it.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sandcrab_protocol::{AUTH_TOKEN_BYTES, AuthToken, ProtocolError, encode_frame};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::client::{BoxedStream, Connector, read_one_response};
use crate::{Result, SandboxError};

/// Default bound on connect + `auth` exchange.
pub const DEFAULT_AUTH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// At most this many `auth` handshakes of one connector at once. `crabd` keeps at most 16
/// unauthenticated connections and evicts the oldest beyond that (its pre-auth queue), so a
/// burst of opens (32 processes spawned at once, each on its own connection) would otherwise
/// have some of them reset mid-handshake. Waiting for a turn counts against the timeout.
pub const MAX_CONCURRENT_HANDSHAKES: usize = 8;

/// A fresh 256-bit token from the OS CSPRNG.
pub fn generate_auth_token() -> Result<AuthToken> {
    let mut bytes = [0u8; AUTH_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| {
        SandboxError::Io(io::Error::other(format!(
            "OS random number generator failed: {e}"
        )))
    })?;
    Ok(AuthToken::from_bytes(&bytes))
}

/// A [`Connector`] that sends the `auth` frame on every new stream and only returns
/// streams `crabd` accepted. A refusal is [`io::ErrorKind::PermissionDenied`].
#[derive(Debug, Clone)]
pub struct AuthConnector {
    inner: Arc<dyn Connector>,
    token: AuthToken,
    timeout: Duration,
    /// Shared by clones: one budget per sandbox (see [`MAX_CONCURRENT_HANDSHAKES`]).
    handshakes: Arc<Semaphore>,
}

impl AuthConnector {
    #[must_use]
    pub fn new(inner: Arc<dyn Connector>, token: AuthToken) -> Self {
        Self {
            inner,
            token,
            timeout: DEFAULT_AUTH_HANDSHAKE_TIMEOUT,
            handshakes: Arc::new(Semaphore::new(MAX_CONCURRENT_HANDSHAKES)),
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn handshake(&self) -> io::Result<BoxedStream> {
        let mut stream = self.inner.connect().await?;
        let frame = encode_frame(&self.token.to_request(0)).map_err(io::Error::other)?;
        stream.write_all(&frame).await?;
        stream.flush().await?;
        let resp = read_one_response(&mut stream)
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string()))?;
        match resp.into_result() {
            Ok(_) => Ok(stream),
            Err(ProtocolError::Unauthorized(_)) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "crabd refused the auth token",
            )),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("auth handshake failed: {e}"),
            )),
        }
    }
}

#[async_trait]
impl Connector for AuthConnector {
    async fn connect(&self) -> io::Result<BoxedStream> {
        let turn_then_handshake = async {
            let _turn = self
                .handshakes
                .acquire()
                .await
                .map_err(|_| io::Error::other("handshake budget closed"))?;
            self.handshake().await
        };
        tokio::time::timeout(self.timeout, turn_then_handshake)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "auth handshake timed out"))?
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use sandcrab_guest::{AuthConfig, ListenAddr, Server, ServerConfig, ToolContext};
    use sandcrab_protocol::{BashResult, codes};
    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::client::{CrabdClient, Endpoint, open_tunnel, wait_ready};
    use crate::testing::bash;

    async fn crabd(token: &AuthToken, cwd: &std::path::Path) -> std::net::SocketAddr {
        let mut config = ServerConfig::new(ListenAddr::Tcp("127.0.0.1:0".into()));
        config.auth = Some(AuthConfig {
            token: token.clone(),
            timeout: Duration::from_millis(500),
        });
        let server = Server::bind(config, Arc::new(ToolContext::new(cwd)))
            .await
            .unwrap();
        let addr = server.tcp_addr().unwrap();
        tokio::spawn(server.run(std::future::pending()));
        addr
    }

    #[tokio::test]
    async fn a_burst_of_connects_stays_within_crabds_pending_queue() {
        let dir = tempfile::tempdir().unwrap();
        let token = generate_auth_token().unwrap();
        let addr = crabd(&token, dir.path()).await;
        let conn = AuthConnector::new(Arc::new(Endpoint::Tcp(addr)), token);
        let n = 4 * sandcrab_guest::preauth::DEFAULT_PENDING;
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..n {
            let conn = conn.clone();
            set.spawn(async move { conn.connect().await.map(|_s| ()) });
        }
        // Every stream is kept open until its task ends, so the handshakes really overlap.
        let failed: Vec<String> = set
            .join_all()
            .await
            .into_iter()
            .filter_map(|r| r.err().map(|e| e.to_string()))
            .collect();
        assert!(
            failed.is_empty(),
            "{} of {n} failed: {failed:?}",
            failed.len()
        );
    }

    #[test]
    fn tokens_are_random_and_well_formed() {
        let a = generate_auth_token().unwrap();
        let b = generate_auth_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.expose().len(), sandcrab_protocol::AUTH_TOKEN_HEX_LEN);
        assert_eq!(AuthToken::parse(a.expose()).unwrap(), a);
    }

    #[tokio::test]
    async fn authenticated_client_tunnel_and_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let token = generate_auth_token().unwrap();
        let addr = crabd(&token, dir.path()).await;
        let conn = AuthConnector::new(Arc::new(Endpoint::Tcp(addr)), token.clone());
        assert!(!format!("{conn:?}").contains(token.expose()));

        let ready = wait_ready(&conn, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        let r: BashResult = ready
            .client
            .call(bash("echo authed"))
            .await
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(r.stdout, "authed\n");

        // A second, independent connection (reconnect) is authenticated too.
        let c2 = CrabdClient::connect(&conn).await.unwrap();
        c2.hello().await.unwrap();

        // tunnel.open rides an authenticated connection.
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = target.accept().await.unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut s, b"tunnel ok")
                .await
                .unwrap();
        });
        let mut t = open_tunnel(&conn, port, Duration::from_secs(5))
            .await
            .unwrap();
        let mut got = vec![0; 9];
        t.read_exact(&mut got).await.unwrap();
        assert_eq!(got, b"tunnel ok");
    }

    #[tokio::test]
    async fn wrong_or_missing_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let token = generate_auth_token().unwrap();
        let addr = crabd(&token, dir.path()).await;

        let wrong = AuthConnector::new(
            Arc::new(Endpoint::Tcp(addr)),
            generate_auth_token().unwrap(),
        );
        let err = wrong.connect().await.err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        let err = wait_ready(&wrong, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refused the auth token"), "{err}");

        // No auth at all: the first request gets UNAUTHORIZED and the connection closes.
        let plain = CrabdClient::connect(&Endpoint::Tcp(addr)).await.unwrap();
        let err = plain.hello().await.unwrap_err();
        assert!(
            matches!(&err, SandboxError::Protocol(e) if e.code() == codes::UNAUTHORIZED),
            "{err:?}"
        );
        let err = open_tunnel(&Endpoint::Tcp(addr), 22, Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&err, SandboxError::Protocol(e) if e.code() == codes::UNAUTHORIZED),
            "{err:?}"
        );
    }
}
