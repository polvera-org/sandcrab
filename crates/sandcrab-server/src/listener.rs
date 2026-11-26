//! The socket (design §5.1, §5.8) and the first frame of every connection (§5.2).
//!
//! The socket's directory must not be writable by anyone but the server's user and root. A
//! stale socket is replaced only when it is a socket of the server's user that nobody
//! answers on. The socket is bound under a temporary name, given its mode and group, and
//! renamed into place, so it never accepts a connection with looser permissions.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sandcrab_sdk::protocol::api::{self, ApiError};
use sandcrab_sdk::protocol::{
    JsonRpcVersion, ProtocolError, RequestId, ResponsePayload, RpcMessage, RpcRequest, RpcResponse,
};
use tokio::net::{UnixListener, UnixStream};

use crate::wire::{frame, read_frame, write_frame};
use crate::{FIRST_FRAME_TIMEOUT, Peer, ServerError, Shared, control, process};

/// The bound socket: its path and the identity of the file the server created, so a stop
/// removes only its own socket, never a successor's.
#[derive(Debug)]
pub(crate) struct BoundSocket {
    pub path: PathBuf,
    ident: (u64, u64),
}

impl BoundSocket {
    /// Remove the socket if it is still the one this server bound.
    pub(crate) fn remove(&self) {
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.ident
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Create the socket directory when it is missing (`0700`, or `0710` with the socket's group
/// so its members can reach the socket), then refuse one others can write.
pub(crate) fn prepare_dir(
    socket: &Path,
    uid: u32,
    gid: Option<u32>,
) -> Result<PathBuf, ServerError> {
    let dir = socket
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| {
            ServerError::Config(format!("socket `{}` has no directory", socket.display()))
        })?
        .to_path_buf();
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .mode(if gid.is_some() { 0o710 } else { 0o700 })
            .create(&dir)
            .map_err(|e| ServerError::io(format!("creating {}", dir.display()), e))?;
        if let Some(gid) = gid {
            std::os::unix::fs::chown(&dir, None, Some(gid))
                .map_err(|e| ServerError::io(format!("chown {}", dir.display()), e))?;
        }
    }
    sandcrab_sdk::core::private_dir::check_foreign_dir(
        &dir,
        uid,
        "the server socket's directory must hold the socket and nothing others can replace",
    )?;
    Ok(dir)
}

/// Remove a stale socket at `path`, or refuse: something that is not a socket, a socket of
/// another user, or a socket another server answers on.
pub(crate) async fn clear_stale(path: &Path, uid: u32) -> Result<(), ServerError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ServerError::io(format!("checking {}", path.display()), e)),
    };
    let refuse = |why: &str| {
        Err(ServerError::Config(format!(
            "socket `{}` {why}; remove it or choose another path",
            path.display()
        )))
    };
    if !meta.file_type().is_socket() {
        return refuse("exists and is not a socket");
    }
    if meta.uid() != uid {
        return refuse(&format!("belongs to uid {}, not this server's", meta.uid()));
    }
    let answer = tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(path)).await;
    if matches!(answer, Ok(Ok(_)) | Err(_)) {
        return refuse("answers: another server is running on it");
    }
    std::fs::remove_file(path)
        .map_err(|e| ServerError::io(format!("removing stale {}", path.display()), e))
}

/// Bind the socket with `mode` and `gid` applied before it is reachable at `path`.
pub(crate) fn bind(
    path: &Path,
    mode: u32,
    gid: Option<u32>,
) -> Result<(UnixListener, BoundSocket), ServerError> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let tmp = dir.join(format!(".s{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let ctx = |what: &str| format!("{what} {}", path.display());
    let std_listener = std::os::unix::net::UnixListener::bind(&tmp)
        .map_err(|e| ServerError::io(ctx("binding"), e))?;
    let finish = || -> std::io::Result<()> {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        if let Some(gid) = gid {
            std::os::unix::fs::chown(&tmp, None, Some(gid))?;
        }
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = finish() {
        let _ = std::fs::remove_file(&tmp);
        return Err(ServerError::io(ctx("setting up"), e));
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| ServerError::io(ctx("checking"), e))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| ServerError::io(ctx("configuring"), e))?;
    let listener = UnixListener::from_std(std_listener)
        .map_err(|e| ServerError::io(ctx("listening on"), e))?;
    Ok((
        listener,
        BoundSocket {
            path: path.to_path_buf(),
            ident: (meta.dev(), meta.ino()),
        },
    ))
}

/// An error answer.
pub(crate) fn failure(id: Option<RequestId>, err: &ApiError) -> RpcResponse {
    RpcResponse {
        jsonrpc: JsonRpcVersion,
        id,
        payload: ResponsePayload::Error(err.to_rpc_error()),
    }
}

/// Answer one frame, then close.
async fn refuse(mut stream: UnixStream, id: Option<RequestId>, err: ApiError) {
    let _ = write_frame(&mut stream, &frame(&failure(id, &err))).await;
}

/// Answer one frame before the peer's first frame was read, then close. The peer is likely
/// writing its first frame at this moment: a close right after the answer would turn that
/// write into `EPIPE` and the peer would never read the answer. So the server half-closes and
/// discards what arrives until the peer closes, at most [`FIRST_FRAME_TIMEOUT`].
async fn refuse_early(mut stream: UnixStream, err: ApiError) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if write_frame(&mut stream, &frame(&failure(None, &err)))
        .await
        .is_err()
    {
        return;
    }
    let _ = stream.shutdown().await;
    let mut buf = [0u8; 4096];
    let drain = async { while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {} };
    let _ = tokio::time::timeout(FIRST_FRAME_TIMEOUT, drain).await;
}

/// Over the connection limit: one `LIMIT_EXCEEDED` frame, then close. Bounded, so a peer
/// that does not read cannot hold the task.
pub(crate) async fn over_limit(stream: UnixStream, limit: u32) {
    let err = ApiError::LimitExceeded(api::LimitExceededData {
        what: "connections".into(),
        limit: u64::from(limit),
    });
    let _ = tokio::time::timeout(Duration::from_secs(1), refuse_early(stream, err)).await;
}

/// A connection whose first frame was `forward.attach`: claim the announced connection,
/// ack, and hand the stream to the waiting target; raw bytes follow. An unknown, used or
/// timed-out `conn_id` (or another uid's) is `-32004`, then close.
async fn attach(shared: &Shared, peer: Peer, mut stream: UnixStream, req: RpcRequest) {
    let p: api::ForwardAttachParams = match api::decode_params(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return refuse(stream, Some(req.id), e.into()).await,
    };
    let Some(tx) = shared.hub.claim(&p.conn_id, peer.uid) else {
        tracing::debug!(uid = peer.uid, "forward.attach for an unknown connection");
        let err = ProtocolError::NotFound("no announced connection with this conn_id".into());
        return refuse(stream, Some(req.id), err.into()).await;
    };
    let ack = RpcResponse::success(
        req.id,
        serde_json::to_value(api::Empty {}).expect("empty object serializes"),
    );
    if write_frame(&mut stream, &frame(&ack)).await.is_ok() {
        // The target may have given up meanwhile: then the stream drops here and closes.
        let _ = tx.send(stream);
    }
}

/// Whether reading the peer's credentials failed only because the peer is already gone.
fn peer_gone(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotConnected | std::io::ErrorKind::ConnectionReset
    )
}

/// One accepted connection: the peer check, then the first frame decides its kind.
pub(crate) async fn connection(shared: Arc<Shared>, mut stream: UnixStream) {
    let peer = match Peer::of(&stream) {
        Ok(p) => p,
        // A peer that already hung up (a readiness probe; macOS answers ENOTCONN) is routine.
        Err(e) if peer_gone(&e) => {
            tracing::debug!(error = %e, "peer closed before its credentials were read");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "no peer credentials; connection closed");
            return;
        }
    };
    if !peer.admitted(&shared.allow_uids) {
        tracing::info!(uid = peer.uid, "connection refused: uid not allowed");
        return refuse_early(stream, ApiError::Unauthorized).await;
    }
    let payload = match tokio::time::timeout(FIRST_FRAME_TIMEOUT, read_frame(&mut stream)).await {
        Err(_) => {
            tracing::debug!(uid = peer.uid, "no first frame in time; connection closed");
            return;
        }
        Ok(Ok(None)) => return,
        Ok(Ok(Some(p))) => p,
        Ok(Err(e)) => return refuse(stream, None, ApiError::Protocol(e.into())).await,
    };
    let req = match RpcMessage::from_slice(&payload) {
        Ok(RpcMessage::Request(req)) => req,
        Ok(_) => {
            let err = ProtocolError::InvalidRequest("the first frame must be a request".into());
            return refuse(stream, RpcMessage::sniff_id(&payload), err.into()).await;
        }
        Err(e) => return refuse(stream, RpcMessage::sniff_id(&payload), e.into()).await,
    };
    match req.method.as_str() {
        api::METHOD_SERVER_HELLO => control::run(shared, peer, stream, req).await,
        api::METHOD_PROCESS_SPAWN => process::run(&shared, peer, stream, req).await,
        api::METHOD_FORWARD_ATTACH => attach(&shared, peer, stream, req).await,
        other => {
            let err = ProtocolError::InvalidRequest(format!(
                "a connection starts with `server.hello`, `process.spawn` or `forward.attach`, \
                 not `{other}`"
            ));
            refuse(stream, Some(req.id), err.into()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid() -> u32 {
        sandcrab_sdk::core::private_dir::current_uid()
    }

    #[test]
    fn socket_dir_is_created_private_and_a_writable_one_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("run/server.sock");
        let dir = prepare_dir(&sock, uid(), None).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = prepare_dir(&open.join("s.sock"), uid(), None).unwrap_err();
        assert!(err.to_string().contains("other users"), "{err}");
        // Group-writable is refused too.
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(prepare_dir(&open.join("s.sock"), uid(), None).is_err());
    }

    #[tokio::test]
    async fn a_peer_gone_before_the_credential_check_is_routine() {
        assert!(!peer_gone(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        drop(std::os::unix::net::UnixStream::connect(&path).unwrap());
        let (stream, _) = listener.accept().await.unwrap();
        // macOS: ENOTCONN once the peer closed. Linux keeps the credentials from connect time.
        match Peer::of(&stream) {
            Err(e) => assert!(peer_gone(&e), "{e}"),
            Ok(_) => assert!(
                !cfg!(target_os = "macos"),
                "macOS answered with credentials"
            ),
        }
    }

    #[tokio::test]
    async fn stale_socket_rules_and_mode_before_accept() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.sock");
        clear_stale(&path, uid()).await.unwrap();

        // A regular file is not replaced.
        std::fs::write(&path, b"x").unwrap();
        assert!(clear_stale(&path, uid()).await.is_err());
        std::fs::remove_file(&path).unwrap();

        // A live socket is not replaced.
        let (live, bound) = bind(&path, 0o660, None).unwrap();
        let mode = std::fs::symlink_metadata(&path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o660);
        let err = clear_stale(&path, uid()).await.unwrap_err();
        assert!(err.to_string().contains("another server"), "{err}");

        // A dead one is.
        drop(live);
        clear_stale(&path, uid()).await.unwrap();
        assert!(!path.exists());

        // A socket of another identity is never removed by the server that bound the first.
        let (_l2, _b2) = bind(&path, 0o600, None).unwrap();
        bound.remove();
        assert!(path.exists());
        // A socket owned by another uid is refused.
        assert!(clear_stale(&path, uid().wrapping_add(1)).await.is_err());
    }
}
