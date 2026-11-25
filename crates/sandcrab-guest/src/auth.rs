//! Server side of the `auth` frame (see [`sandcrab_protocol::auth`]).
//!
//! With a token configured ([`crate::ServerConfig::auth`]), every accepted connection must
//! send exactly one valid `auth` request as its first frame within
//! [`AuthConfig::timeout`]. Anything else (wrong or missing token, malformed or non-`auth`
//! first frame, EOF, timeout) gets one generic `UNAUTHORIZED` error frame and the connection
//! is closed before any other frame is read: no `hello`, tool or `tunnel.open` runs. The
//! token is compared in constant time and never logged.

use std::{fs, io, path::Path, time::Duration};

use sandcrab_protocol::{
    AuthAck, AuthParams, AuthToken, ProtocolError, RequestId, RpcMessage, RpcResponse,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, warn};

use crate::codec;

/// Default time a fresh connection has to authenticate. The host sends `auth` in its first
/// write, so this only bounds how long an unauthenticated peer can hold a pending place.
pub const DEFAULT_AUTH_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on writing the (tiny) verdict frame to a client that does not read.
const VERDICT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Per-server authentication settings.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub token: AuthToken,
    /// How long a new connection may take to send its `auth` frame.
    pub timeout: Duration,
}

impl AuthConfig {
    #[must_use]
    pub fn new(token: AuthToken) -> Self {
        Self {
            token,
            timeout: DEFAULT_AUTH_TIMEOUT,
        }
    }
}

/// Why a connection was refused (for logs; never sent to the peer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Timeout,
    Eof,
    Malformed,
    NotAuth,
    WrongToken,
}

/// Run the `auth` exchange on a fresh connection. `Ok(())` = authenticated; the stream is
/// positioned right after the `auth` frame.
pub async fn authenticate<S>(stream: &mut S, cfg: &AuthConfig) -> Result<(), Refusal>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let verdict = match tokio::time::timeout(cfg.timeout, read_auth(stream, &cfg.token)).await {
        Ok(v) => v,
        Err(_) => Err((None, Refusal::Timeout)),
    };
    let (resp, outcome) = match verdict {
        Ok(id) => (
            RpcResponse::success(id, AuthAck::default().to_value()),
            Ok(()),
        ),
        Err((id, why)) => (
            RpcResponse::failure(
                id,
                &ProtocolError::Unauthorized("authentication required".into()),
            ),
            Err(why),
        ),
    };
    let wrote =
        tokio::time::timeout(VERDICT_WRITE_TIMEOUT, codec::write_frame(stream, &resp)).await;
    match (outcome, wrote) {
        (Ok(()), Ok(Ok(()))) => Ok(()),
        // Could not even confirm success: treat as a dead connection.
        (Ok(()), _) => Err(Refusal::Eof),
        (Err(why), _) => Err(why),
    }
}

async fn read_auth<S>(
    stream: &mut S,
    token: &AuthToken,
) -> Result<RequestId, (Option<RequestId>, Refusal)>
where
    S: AsyncRead + Unpin,
{
    let payload = match codec::read_frame(stream).await {
        Ok(Some(p)) => p,
        Ok(None) => return Err((None, Refusal::Eof)),
        Err(_) => return Err((None, Refusal::Malformed)),
    };
    let Ok(RpcMessage::Request(req)) = RpcMessage::from_slice(&payload) else {
        return Err((None, Refusal::Malformed));
    };
    match AuthParams::from_request(&req) {
        Ok(p) if token.verify(&p.token) => Ok(req.id),
        Ok(_) => Err((Some(req.id), Refusal::WrongToken)),
        Err(_) => Err((Some(req.id), Refusal::NotAuth)),
    }
}

/// Read and validate the token file, then try to delete it so tool processes started later
/// cannot read it (they run as the same uid). A read-only mount leaves it in place.
pub fn load_token_file(path: &Path) -> io::Result<AuthToken> {
    let raw = fs::read_to_string(path).map_err(|e| with_path(path, e))?;
    let token = AuthToken::parse(&raw).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })?;
    match fs::remove_file(path) {
        Ok(()) => info!(path = %path.display(), "auth token loaded; token file removed"),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "auth token loaded; could not remove the token file")
        }
    }
    Ok(token)
}

fn with_path(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandcrab_protocol::{AUTH_TOKEN_BYTES, ResponsePayload, RpcRequest, codes};
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    fn cfg() -> AuthConfig {
        AuthConfig {
            token: AuthToken::from_bytes(&[7; AUTH_TOKEN_BYTES]),
            timeout: Duration::from_millis(200),
        }
    }

    async fn run(
        send: impl FnOnce(&mut DuplexStream) -> Vec<u8>,
    ) -> (Result<(), Refusal>, Option<RpcResponse>) {
        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        let bytes = send(&mut client);
        client.write_all(&bytes).await.unwrap();
        let c = cfg();
        let res = authenticate(&mut server, &c).await;
        drop(server);
        let resp = codec::read_frame(&mut client)
            .await
            .ok()
            .flatten()
            .map(|p| serde_json::from_slice(&p).unwrap());
        (res, resp)
    }

    fn frame(req: &RpcRequest) -> Vec<u8> {
        sandcrab_protocol::encode_frame(req).unwrap()
    }

    fn is_unauthorized(resp: &RpcResponse) -> bool {
        matches!(&resp.payload, ResponsePayload::Error(e) if e.code == codes::UNAUTHORIZED)
    }

    #[tokio::test]
    async fn correct_token_is_acked() {
        let (res, resp) = run(|_| frame(&cfg().token.to_request(0))).await;
        assert_eq!(res, Ok(()));
        let resp = resp.unwrap();
        assert_eq!(resp.id, Some(RequestId::Number(0)));
        assert_eq!(resp.payload, ResponsePayload::Result(json!({})));
    }

    #[tokio::test]
    async fn wrong_token_is_refused() {
        let other = AuthToken::from_bytes(&[8; AUTH_TOKEN_BYTES]);
        let (res, resp) = run(|_| frame(&other.to_request(0))).await;
        assert_eq!(res, Err(Refusal::WrongToken));
        assert!(is_unauthorized(&resp.unwrap()));
    }

    #[tokio::test]
    async fn missing_token_is_refused() {
        // A tool call / hello / tunnel.open first, or an auth frame without a token.
        for req in [
            RpcRequest::new(1, "bash", Some(json!({"command": "id"}))),
            RpcRequest::new(1, "hello", None),
            RpcRequest::new(1, "tunnel.open", Some(json!({"port": 80}))),
            RpcRequest::new(1, "auth", Some(json!({}))),
            RpcRequest::new(1, "auth", Some(json!({"token": ""}))),
        ] {
            let (res, resp) = run(|_| frame(&req)).await;
            assert!(res.is_err(), "{req:?}");
            assert!(is_unauthorized(&resp.unwrap()), "{req:?}");
        }
    }

    #[tokio::test]
    async fn garbage_eof_and_silence_are_refused() {
        let (res, _) = run(|_| b"\x00\x00\x00\x02{]".to_vec()).await;
        assert_eq!(res, Err(Refusal::Malformed));
        let (res, _) = run(|_| u32::MAX.to_be_bytes().to_vec()).await;
        assert_eq!(res, Err(Refusal::Malformed));

        let (mut client, mut server) = tokio::io::duplex(1024);
        client.shutdown().await.unwrap();
        assert_eq!(authenticate(&mut server, &cfg()).await, Err(Refusal::Eof));

        let (_client, mut server) = tokio::io::duplex(1024);
        let started = std::time::Instant::now();
        assert_eq!(
            authenticate(&mut server, &cfg()).await,
            Err(Refusal::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn token_file_is_loaded_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("token");
        let t = cfg().token;
        fs::write(&p, format!("{}\n", t.expose())).unwrap();
        assert_eq!(load_token_file(&p).unwrap(), t);
        assert!(!p.exists());
        fs::write(&p, "short").unwrap();
        let err = load_token_file(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!err.to_string().contains("short"), "{err}");
        assert!(load_token_file(&dir.path().join("missing")).is_err());
    }
}
