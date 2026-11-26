//! Control connections and forwards ends at the client (design §4.4, §5.3).
//!
//! The [`Hub`] knows every open control connection (its uid and its frame writer), which one
//! serves the forwards of each sandbox, and the forwarded connections announced and not yet
//! attached.
//!
//! A program in a sandbox connects to a forward port; the sandbox's `ForwardAcceptor` asks
//! the server's target ([`Hub::incoming`]) for a stream. The hub sends `forward.incoming
//! {conn_id, sandbox_id, guest_port}` on the serving control connection and waits, at most
//! [`ATTACH_TIMEOUT`], for a connection whose first frame is `forward.attach {conn_id}` from
//! the same uid. That connection, after its ack, is the stream: raw bytes both ways. No
//! serving connection, a lost one, a timeout or `forward.reject`: the target fails, the
//! acceptor rejects and the guest closes the program's connection (fail closed, counted as
//! `rejected` or `unclaimed`).

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use sandcrab_sdk::SandboxId;
use sandcrab_sdk::protocol::api::{self, ForwardIncoming};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::wire::frame;

/// How long an announced connection waits for its `forward.attach`. The guest closes an
/// unclaimed connection after 10 s; the acceptor gives the target as long. Waiting a little
/// less leaves the guest's accept room within its deadline.
pub(crate) const ATTACH_TIMEOUT: Duration = Duration::from_secs(9);

/// A control connection, as the hub sees it.
#[derive(Clone)]
struct Control {
    uid: u32,
    tx: mpsc::Sender<Vec<u8>>,
}

/// A forwarded connection announced to a client and not attached yet.
struct Pending {
    uid: u32,
    control: u64,
    tx: oneshot::Sender<UnixStream>,
}

#[derive(Default)]
struct State {
    controls: HashMap<u64, Control>,
    /// Sandbox → the control connection that serves its forwards.
    serving: HashMap<SandboxId, u64>,
    pending: HashMap<String, Pending>,
}

pub(crate) struct Hub {
    next: AtomicU64,
    state: Mutex<State>,
    /// Forwarded connections from announcement to the relay's end, all sandboxes.
    forwarded: Arc<Semaphore>,
    max_forwarded: u32,
}

/// An attached forwarded connection; it holds its place under the server-wide cap until the
/// relay drops it.
pub(crate) struct Forwarded {
    stream: UnixStream,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for Forwarded {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Forwarded {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// Unregisters a control connection when dropped (also when its task is aborted).
pub(crate) struct ControlGuard {
    hub: Arc<Hub>,
    pub id: u64,
}

impl Drop for ControlGuard {
    fn drop(&mut self) {
        self.hub.unregister(self.id);
    }
}

/// Removes an announcement when the target's wait ends, however it ends.
struct PendingGuard<'a> {
    hub: &'a Hub,
    conn_id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.hub.lock().pending.remove(&self.conn_id);
    }
}

/// 128 random bits, hex.
fn conn_id() -> io::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| io::Error::other(format!("no randomness: {e}")))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

impl Hub {
    /// A hub that relays at most `max_forwarded` forwarded connections at once.
    pub(crate) fn new(max_forwarded: u32) -> Self {
        Self {
            next: AtomicU64::new(0),
            state: Mutex::default(),
            forwarded: Arc::new(Semaphore::new(max_forwarded as usize)),
            max_forwarded,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register a control connection of `uid` whose frames go to `tx`.
    pub(crate) fn register(self: &Arc<Self>, uid: u32, tx: mpsc::Sender<Vec<u8>>) -> ControlGuard {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.lock().controls.insert(id, Control { uid, tx });
        ControlGuard {
            hub: Arc::clone(self),
            id,
        }
    }

    /// A control connection is gone: the forwards it served are unserved, and the
    /// connections announced on it are refused now.
    fn unregister(&self, id: u64) {
        let mut st = self.lock();
        st.controls.remove(&id);
        st.serving.retain(|_, c| *c != id);
        st.pending.retain(|_, p| p.control != id);
    }

    /// Forget every control connection (server stop): their writers end and close them.
    pub(crate) fn clear(&self) {
        *self.lock() = State::default();
    }

    /// Control connection `control` now serves the forwards of `sandbox`.
    pub(crate) fn serve(&self, sandbox: SandboxId, control: u64) {
        self.lock().serving.insert(sandbox, control);
    }

    pub(crate) fn is_served(&self, sandbox: SandboxId) -> bool {
        let st = self.lock();
        st.serving
            .get(&sandbox)
            .is_some_and(|c| st.controls.contains_key(c))
    }

    /// The sandbox is gone.
    pub(crate) fn forget(&self, sandbox: SandboxId) {
        self.lock().serving.remove(&sandbox);
    }

    fn senders(&self, uid: Option<u32>) -> Vec<mpsc::Sender<Vec<u8>>> {
        self.lock()
            .controls
            .values()
            .filter(|c| uid.is_none_or(|u| u == c.uid))
            .map(|c| c.tx.clone())
            .collect()
    }

    /// A notification on every control connection (of `uid`, if given). Never waits on a
    /// slow client: a full queue drops the notification for that connection.
    pub(crate) fn notify<T: serde::Serialize>(&self, uid: Option<u32>, method: &str, params: &T) {
        let bytes = frame(&api::notification(method, params));
        for tx in self.senders(uid) {
            let _ = tx.try_send(bytes.clone());
        }
    }

    /// The forward target: announce the connection to the serving client and wait for its
    /// attach. Any failure refuses the connection.
    pub(crate) async fn incoming(
        &self,
        sandbox: SandboxId,
        guest_port: u16,
    ) -> io::Result<Forwarded> {
        let refuse = |why: &str| io::Error::new(io::ErrorKind::ConnectionRefused, why.to_owned());
        let Ok(permit) = Arc::clone(&self.forwarded).try_acquire_owned() else {
            tracing::warn!(%sandbox, guest_port, limit = self.max_forwarded,
                "forwarded connection refused: too many forwarded connections");
            return Err(refuse("too many forwarded connections on this server"));
        };
        let conn_id = conn_id()?;
        let (tx, rx) = oneshot::channel();
        let control = {
            let mut st = self.lock();
            let Some(id) = st.serving.get(&sandbox).copied() else {
                tracing::debug!(%sandbox, guest_port, "forwarded connection refused: not served");
                return Err(refuse("the forwards of this sandbox are not served"));
            };
            let Some(control) = st.controls.get(&id).cloned() else {
                return Err(refuse("the serving control connection is gone"));
            };
            st.pending.insert(
                conn_id.clone(),
                Pending {
                    uid: control.uid,
                    control: id,
                    tx,
                },
            );
            control
        };
        let _guard = PendingGuard {
            hub: self,
            conn_id: conn_id.clone(),
        };
        let note = frame(&api::notification(
            api::NOTIFY_FORWARD_INCOMING,
            &ForwardIncoming {
                conn_id: conn_id.clone(),
                sandbox_id: sandbox.to_string(),
                guest_port,
            },
        ));
        let wait = async {
            control
                .tx
                .send(note)
                .await
                .map_err(|_| refuse("the serving control connection is gone"))?;
            rx.await
                .map_err(|_| refuse("the client rejected the connection or went away"))
        };
        match tokio::time::timeout(ATTACH_TIMEOUT, wait).await {
            Ok(r) => r.map(|stream| Forwarded {
                stream,
                _permit: permit,
            }),
            Err(_) => {
                tracing::debug!(%sandbox, guest_port, "forwarded connection refused: not attached in time");
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "not attached in time",
                ))
            }
        }
    }

    /// Claim an announced connection for an attach from `uid`. `None`: unknown, used, or
    /// another uid's.
    pub(crate) fn claim(&self, conn_id: &str, uid: u32) -> Option<oneshot::Sender<UnixStream>> {
        let mut st = self.lock();
        if st.pending.get(conn_id).is_some_and(|p| p.uid == uid) {
            st.pending.remove(conn_id).map(|p| p.tx)
        } else {
            None
        }
    }

    /// `forward.reject` from `uid`: refuse the announced connection now.
    pub(crate) fn reject(&self, conn_id: &str, uid: u32) {
        drop(self.claim(conn_id, uid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M3: forwarded connections count against a server-wide cap from announcement to the
    /// relay's end; one past it is refused at once, and a place frees when a relay ends.
    #[tokio::test]
    async fn forwarded_connections_are_capped_server_wide() {
        let hub = Arc::new(Hub::new(1));
        let (tx, mut rx) = mpsc::channel(8);
        let control = hub.register(1000, tx);
        let sandbox = SandboxId::new();
        hub.serve(sandbox, control.id);

        let first = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move { hub.incoming(sandbox, 80).await }
        });
        let note: serde_json::Value = {
            let f = rx.recv().await.unwrap();
            serde_json::from_slice(&f[4..]).unwrap()
        };
        let conn_id = note["params"]["conn_id"].as_str().unwrap().to_owned();

        let e = hub.incoming(sandbox, 80).await.err().unwrap();
        assert!(e.to_string().contains("too many forwarded"), "{e}");

        let (a, _b) = UnixStream::pair().unwrap();
        hub.claim(&conn_id, 1000).unwrap().send(a).unwrap();
        let relay = first.await.unwrap().unwrap();
        let e = hub.incoming(sandbox, 80).await.err().unwrap();
        assert!(
            e.to_string().contains("too many forwarded"),
            "still held: {e}"
        );
        drop(relay);
        // The place is free again: this one is announced (and then times out unattached).
        let again = tokio::spawn({
            let hub = Arc::clone(&hub);
            async move { hub.incoming(sandbox, 80).await }
        });
        assert!(rx.recv().await.is_some(), "announced");
        again.abort();
    }
}
