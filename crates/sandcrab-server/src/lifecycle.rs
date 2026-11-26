//! Start and stop (design §5.7, §5.10).
//!
//! Start: check the socket directory and a stale socket, build the mount policy with the
//! server's own directories protected, build the backend, remove what a dead predecessor
//! left (Docker: owned containers only), then bind. Stop: refuse new requests, destroy every
//! sandbox, remove the socket.

use std::future::Future;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use std::time::Duration;

use sandcrab_sdk::protocol::api;
use sandcrab_sdk::{MountPolicy, MountRoot, SandcrabBuilder};
use tokio::task::JoinSet;

use crate::config::resolve_group;
use crate::control::core_mode;
use crate::process::SHUTDOWN_GRACE_MS;
use crate::{MAX_CONNECTIONS, Server, ServerConfig, ServerError, Shared, listener};

pub(crate) async fn start(
    mut config: ServerConfig,
    builder: SandcrabBuilder,
    mut protected: Vec<PathBuf>,
) -> Result<Server, ServerError> {
    config.validate()?;
    let uid = sandcrab_sdk::core::private_dir::current_uid();
    let socket = config.socket_path()?.to_path_buf();
    let gid = config
        .socket_group
        .as_deref()
        .map(resolve_group)
        .transpose()?;

    let socket_dir = listener::prepare_dir(&socket, uid, gid)?;
    listener::clear_stale(&socket, uid).await?;

    let state_dir = match &config.state_dir {
        Some(d) => d.clone(),
        None => sandcrab_sdk::core::default_state_root()?,
    };
    config.state_dir = Some(state_dir.clone());
    protected.push(socket_dir);
    protected.push(state_dir.clone());
    if let Ok(pins) = sandcrab_sdk::core::default_pins_dir() {
        protected.push(pins);
    }
    let roots = config
        .mount_roots
        .iter()
        .map(|r| MountRoot {
            path: r.path.clone(),
            max_mode: core_mode(r.mode),
        })
        .collect();
    let policy = MountPolicy::new(roots, protected)?;

    let mut builder = builder.state_dir(&state_dir).mount_policy(policy);
    for d in &config.templates_dirs {
        builder = builder.templates_dir(d);
    }
    let sandcrab = builder.build()?;
    let expected = config.backend.map(crate::BackendName::as_str);
    let kind = sandcrab.backend_kind().to_string();
    if expected != Some(kind.as_str()) {
        return Err(ServerError::Config(format!(
            "the config names backend `{}` but `{kind}` was built",
            expected.unwrap_or("none")
        )));
    }

    remove_predecessor(&sandcrab).await?;

    let (listener, bound) = listener::bind(&socket, config.socket_mode, gid)?;
    let allow = config.allowed_uids(uid);
    tracing::info!(
        socket = %socket.display(),
        backend = %kind,
        allow_uids = ?allow,
        "sandcrab server listening"
    );
    Ok(Server {
        shared: Arc::new(Shared::new(sandcrab, config, allow)),
        listener,
        socket: bound,
    })
}

/// Docker: containers of a dead predecessor (same uid, same state directory, owning process
/// gone) keep running with nothing in them. Remove them. libkrun's backend removes leftover
/// run directories itself when it is built; its VMs died with their owner.
#[allow(clippy::unused_async)]
async fn remove_predecessor(sandcrab: &sandcrab_sdk::Sandcrab) -> Result<(), ServerError> {
    #[cfg(feature = "docker")]
    if sandcrab.docker_backend().is_some() {
        let report = sandcrab
            .reap_orphans(sandcrab_sdk::ReapScope::Owned)
            .await?;
        if !report.containers.is_empty() {
            tracing::info!(
                containers = report.containers.len(),
                "removed sandboxes a previous server left"
            );
        }
    }
    let _ = sandcrab;
    Ok(())
}

pub(crate) async fn run(
    server: Server,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServerError> {
    let Server {
        shared,
        listener,
        socket,
    } = server;
    let mut conns = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    if conns.len() >= MAX_CONNECTIONS as usize {
                        tracing::warn!(limit = MAX_CONNECTIONS, "connection refused: too many connections");
                        tokio::spawn(listener::over_limit(stream, MAX_CONNECTIONS));
                        continue;
                    }
                    conns.spawn(listener::connection(Arc::clone(&shared), stream));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            },
        }
    }

    tracing::info!("sandcrab server stopping: cancelling processes, destroying every sandbox");
    shared.shutting_down.store(true, Ordering::SeqCst);
    drop(listener);
    shared.hub.notify(
        None,
        api::NOTIFY_SERVER_SHUTDOWN,
        &api::ServerShutdown {
            grace_ms: SHUTDOWN_GRACE_MS,
        },
    );
    // Cancels answer when the trees are gone; then give the relays a moment to pass the
    // `EXIT` records on before the connections are cut.
    let bound = Duration::from_millis(SHUTDOWN_GRACE_MS) + Duration::from_secs(2);
    shared.processes.cancel_all(SHUTDOWN_GRACE_MS, bound).await;
    shared.processes.drained(Duration::from_secs(1)).await;
    let result = shared.sandcrab.shutdown().await;
    conns.abort_all();
    // Their writers end and close the control connections.
    shared.hub.clear();
    socket.remove();
    result.map_err(ServerError::from)
}

/// Resolves when `reader` reaches end-of-file or fails. Read on a plain thread, so a reader
/// that never ends does not hold up the runtime's exit. The bytes are discarded unread.
pub fn lifeline<R: Read + Send + 'static>(mut reader: R) -> impl Future<Output = ()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        let _ = tx.send(());
    });
    async move {
        let _ = rx.await;
    }
}

/// [`lifeline`] on this process's stdin (`--lifeline-stdin`): the client that started the
/// server holds the pipe; when it dies, the server stops and its sandboxes go with it.
pub fn stdin_lifeline() -> impl Future<Output = ()> {
    lifeline(std::io::stdin())
}
