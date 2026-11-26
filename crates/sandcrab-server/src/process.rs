//! Process connections (design §5.4) and `process.cancel`.
//!
//! A connection whose first frame is `process.spawn` carries one process. The server checks
//! the params and that the peer's uid owns the sandbox, registers the `process_id`, opens
//! `process.open` in the guest with the same id and values, answers after the guest's ack,
//! then relays records unchanged both ways: it checks each record's kind and length and
//! decodes nothing else (the `EXIT` payload is parsed and written again, field for field).
//!
//! | What happened | The client gets |
//! | --- | --- |
//! | Bad params, not the owner, a `process_id` in use, over the process limit, shutting down | One error frame, then close (`-32602`, `SANDBOX_NOT_FOUND`, `CONFLICT`, `LIMIT_EXCEEDED`, `SHUTTING_DOWN`) |
//! | A cancel came before the guest open was written | `CANCELLED` (`-32012`); nothing ran |
//! | Nothing could be sent to the guest | `UNAVAILABLE`; nothing ran |
//! | The guest refused (`SPAWN_FAILED`, its `LIMIT_EXCEEDED`, `CANCELLED`, `-32602`) | `crabd`'s error, unchanged |
//! | The guest connection ended after the request and before the ack, or before `EXIT` | The connection closes without an answer or without `EXIT`: lost |
//! | The program ran | The ack `{process_id}`, records, `EXIT` last, then close |
//!
//! **Backpressure.** The server reads the next record from one side only after it wrote the
//! previous one to the other, so it holds at most one record per direction. A client that
//! does not read stops the program; a program that does not read stdin stops the client's
//! writes. Each process has its own task, so a slow side never blocks another connection.
//!
//! **A process lives as long as its connection.** A client that closes or loses it (also
//! while a stdin write waits on the program) closes the guest connection: `crabd` stops the
//! tree (ask, `kill_grace_ms`, force). A lost control connection stops nothing.
//!
//! **Cancel cannot lose a race with spawn.** The id is in the table when the request was
//! read, before any await. A cancel for an id that is not open in the guest yet is
//! remembered and the spawn is answered `CANCELLED`; once the open is under way, the cancel
//! goes to the sandbox (core and `crabd` apply the same rule). A cancel for an id the server
//! has not seen is remembered for 60 s (the spawn may still be on its way).
//!
//! **Logging (§12.4).** Never the params as a whole, env values or stdin: the process id,
//! the sandbox, `argv[0]` and the number of env variables.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use sandcrab_sdk::SandboxError;
use sandcrab_sdk::core::{ProcessEvent, ProcessOutput, ProcessStdin, Sandbox};
use sandcrab_sdk::protocol::api::{
    ApiError, DetailData, LimitExceededData, ProcessSpawnAck, ProcessSpawnParams,
};
use sandcrab_sdk::protocol::{
    ProcessCancelParams, ProcessCancelResult, ProtocolError, RECORD_HEADER_LEN, RecordChannel,
    RecordKind, RequestId, RpcRequest, RpcResponse, StdinMode, encode_record, parse_record_header,
    record::MAX_RECORD_DATA_LEN,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::task::JoinSet;

use crate::control::{map_err, owned};
use crate::listener::failure;
use crate::wire::{frame, write_frame};
use crate::{MAX_PROCESSES_PER_SANDBOX, Peer, Shared};

/// How long a cancel for an id the server has not seen is remembered.
const EARLY_CANCEL_TTL: Duration = Duration::from_secs(60);
/// At most this many remembered cancels; the oldest go first.
const MAX_EARLY_CANCELS: usize = 1024;
/// Grace of the cancels a server stop sends.
pub(crate) const SHUTDOWN_GRACE_MS: u64 = 5000;
/// How often a stdin write that waits on the program looks for a client that went away.
const PEER_POLL: Duration = Duration::from_millis(100);
/// After `EXIT`, how long the server waits for the client to close before it closes.
const EXIT_LINGER: Duration = Duration::from_secs(2);

struct Entry {
    uid: u32,
    sandbox: Sandbox,
    /// The open in the guest is under way (or done): cancels go to the sandbox.
    opened: bool,
    /// A cancel came before the open: the spawn answers `CANCELLED`.
    cancelled: bool,
}

/// Every process the server has registered, by `process_id`.
#[derive(Default)]
pub(crate) struct Processes {
    table: Mutex<HashMap<String, Entry>>,
    early: Mutex<HashMap<(u32, String), Instant>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes a table entry when the process connection ends, however it ends.
struct Registration<'a> {
    procs: &'a Processes,
    id: String,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        lock(&self.procs.table).remove(&self.id);
    }
}

impl Processes {
    /// Register `id` for `uid` in `sandbox`, or refuse: a remembered cancel, an id in use,
    /// the per-sandbox limit.
    fn register(
        &self,
        uid: u32,
        id: &str,
        sandbox: &Sandbox,
    ) -> Result<Registration<'_>, ApiError> {
        // Lock order: table, then early (as in `cancel`), so a cancel and a spawn of the
        // same id cannot pass each other.
        let mut t = lock(&self.table);
        let early = lock(&self.early)
            .remove(&(uid, id.to_owned()))
            .is_some_and(|at| at.elapsed() < EARLY_CANCEL_TTL);
        if early {
            return Err(cancelled());
        }
        if t.contains_key(id) {
            return Err(ApiError::Conflict(DetailData {
                detail: format!("process_id {id} is already in use"),
            }));
        }
        let in_sandbox = t
            .values()
            .filter(|e| e.sandbox.id() == sandbox.id())
            .count();
        if in_sandbox >= MAX_PROCESSES_PER_SANDBOX as usize {
            return Err(ApiError::LimitExceeded(LimitExceededData {
                what: "processes".into(),
                limit: u64::from(MAX_PROCESSES_PER_SANDBOX),
            }));
        }
        t.insert(
            id.to_owned(),
            Entry {
                uid,
                sandbox: sandbox.clone(),
                opened: false,
                cancelled: false,
            },
        );
        Ok(Registration {
            procs: self,
            id: id.to_owned(),
        })
    }

    /// Mark the open as under way, unless a cancel came first.
    fn start_open(&self, id: &str) -> Result<(), ApiError> {
        let mut t = lock(&self.table);
        let e = t.get_mut(id).expect("registered");
        if e.cancelled {
            return Err(cancelled());
        }
        e.opened = true;
        Ok(())
    }

    /// `process.cancel` from `uid`. Idempotent; an unknown (or another uid's) id answers
    /// `was_running: false` and is remembered for a spawn still on its way.
    pub(crate) async fn cancel(
        &self,
        uid: u32,
        p: ProcessCancelParams,
    ) -> Result<ProcessCancelResult, ApiError> {
        let not_running = ProcessCancelResult {
            was_running: false,
            tree_clean: true,
        };
        let sandbox = {
            let mut t = lock(&self.table);
            match t.get_mut(&p.process_id) {
                Some(e) if e.uid == uid => {
                    if !e.opened {
                        e.cancelled = true;
                        return Ok(not_running);
                    }
                    e.sandbox.clone()
                }
                _ => {
                    // Still under the table lock: a spawn registering now sees it.
                    self.remember(uid, p.process_id);
                    drop(t);
                    return Ok(not_running);
                }
            }
        };
        match sandbox.cancel_process(&p.process_id, p.grace_ms).await {
            Ok(r) => Ok(r),
            // A destroyed sandbox took the tree with it.
            Err(_) if sandbox.is_destroyed() => Ok(not_running),
            Err(SandboxError::Protocol(e)) => Err(ApiError::Protocol(e)),
            Err(e) => Err(map_err(e, "")),
        }
    }

    fn remember(&self, uid: u32, id: String) {
        let mut early = lock(&self.early);
        early.retain(|_, at| at.elapsed() < EARLY_CANCEL_TTL);
        if early.len() >= MAX_EARLY_CANCELS
            && let Some(oldest) = early
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(k, _)| k.clone())
        {
            early.remove(&oldest);
        }
        early.insert((uid, id), Instant::now());
    }

    /// Cancel every process (server stop) with `grace_ms`, and wait at most `bound`.
    pub(crate) async fn cancel_all(&self, grace_ms: u64, bound: Duration) {
        let mut set = JoinSet::new();
        {
            let mut t = lock(&self.table);
            for (id, e) in t.iter_mut() {
                if e.opened {
                    let sb = e.sandbox.clone();
                    let id = id.clone();
                    set.spawn(async move { sb.cancel_process(&id, Some(grace_ms)).await });
                } else {
                    e.cancelled = true;
                }
            }
        }
        let _ =
            tokio::time::timeout(bound, async { while set.join_next().await.is_some() {} }).await;
    }

    /// Wait (at most `bound`) until no process connection is left.
    pub(crate) async fn drained(&self, bound: Duration) {
        let deadline = Instant::now() + bound;
        while !lock(&self.table).is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn cancelled() -> ApiError {
    ApiError::Protocol(ProtocolError::Cancelled(
        "cancelled before the program started".into(),
    ))
}

/// A refusal of a guest open, as the client sees it. `None`: lost (close, no answer).
fn open_error(e: SandboxError) -> Option<ApiError> {
    match e {
        SandboxError::Lost(_) => None,
        SandboxError::Unavailable(m) => Some(ApiError::Unavailable(DetailData { detail: m })),
        SandboxError::Protocol(p) => Some(ApiError::Protocol(p)),
        SandboxError::AlreadyExists { .. } => Some(ApiError::Conflict(DetailData {
            detail: "process_id is already in use".into(),
        })),
        other => Some(map_err(other, "")),
    }
}

/// One error frame for the spawn request `id`.
fn refusal(id: &RequestId, uid: u32, e: &ApiError) -> Vec<u8> {
    tracing::debug!(uid, code = e.code(), "process.spawn refused");
    frame(&failure(Some(id.clone()), e))
}

/// Serve a connection whose first frame was `process.spawn`.
pub(crate) async fn run(shared: &Shared, peer: Peer, stream: UnixStream, req: RpcRequest) {
    let (mut rd, mut wr) = stream.into_split();
    let id = req.id.clone();
    let params = match ProcessSpawnParams::from_request(&req) {
        Ok(p) => p,
        Err(e) => {
            let _ = write_frame(&mut wr, &refusal(&id, peer.uid, &e.into())).await;
            return;
        }
    };
    drop(req);
    let ProcessSpawnParams { sandbox_id, open } = params;
    let pid = open.process_id.clone();
    let pipe = open.stdin == StdinMode::Pipe;
    let setup = || -> Result<(Sandbox, Registration<'_>), ApiError> {
        if shared
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ApiError::ShuttingDown);
        }
        let sid = owned(shared, peer, &sandbox_id)?;
        let sb = shared
            .sandcrab
            .manager()
            .get(sid)
            .map_err(|_| crate::control::not_found(&sandbox_id))?;
        let reg = shared.processes.register(peer.uid, &pid, &sb)?;
        Ok((sb, reg))
    };
    let (sandbox, reg) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = write_frame(&mut wr, &refusal(&id, peer.uid, &e)).await;
            return;
        }
    };
    tracing::debug!(
        uid = peer.uid,
        sandbox = %sandbox.id(),
        process = %pid,
        program = %open.argv[0],
        env_vars = open.env.len(),
        "process.spawn"
    );
    if let Err(e) = shared.processes.start_open(&pid) {
        let _ = write_frame(&mut wr, &refusal(&id, peer.uid, &e)).await;
        return;
    }
    let stream = match sandbox.open_process(open).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(api) = open_error(e) {
                let _ = write_frame(&mut wr, &refusal(&id, peer.uid, &api)).await;
            } else {
                tracing::debug!(process = %pid, "process lost before its ack");
            }
            return;
        }
    };
    let ack = ProcessSpawnAck {
        process_id: pid.clone(),
    };
    let ack = RpcResponse::success(id, serde_json::to_value(ack).expect("ack serializes"));
    if write_frame(&mut wr, &frame(&ack)).await.is_err() {
        return;
    }
    let (stdin, output) = stream.into_split();
    let end = relay(&mut rd, wr, stdin, output, pipe).await;
    drop(reg);
    if end == "exit" {
        // The client may still be writing stdin. Closing under that write would fail it
        // (`EPIPE`) before it read `EXIT`; discard until it closes, briefly.
        let _ = tokio::time::timeout(EXIT_LINGER, discard(&mut rd)).await;
    }
    tracing::debug!(process = %pid, sandbox = %sandbox.id(), end, "process connection closed");
}

/// Relay until `EXIT` was written (then close) or either side failed (then drop both, which
/// stops the tree). Returns how it ended, for the log.
async fn relay(
    rd: &mut OwnedReadHalf,
    mut wr: OwnedWriteHalf,
    mut stdin: ProcessStdin,
    mut output: ProcessOutput,
    pipe: bool,
) -> &'static str {
    let to_client = async {
        loop {
            match output.next().await {
                Ok(Some(ProcessEvent::Stdout(d))) => {
                    write_record(&mut wr, RecordKind::Stdout, &d).await?;
                }
                Ok(Some(ProcessEvent::Stderr(d))) => {
                    write_record(&mut wr, RecordKind::Stderr, &d).await?;
                }
                Ok(Some(ProcessEvent::Exit(exit))) => {
                    wr.write_all(&exit.encode_record()).await?;
                    wr.flush().await?;
                    let _ = wr.shutdown().await;
                    return Ok("exit");
                }
                // The guest connection ended without `EXIT`: close without one (lost).
                Ok(None) | Err(_) => return Ok::<_, std::io::Error>("lost"),
            }
        }
    };
    let from_client = async {
        let mut payload = vec![0u8; MAX_RECORD_DATA_LEN];
        loop {
            let mut header = [0u8; RECORD_HEADER_LEN];
            if rd.read_exact(&mut header).await.is_err() {
                return "client closed";
            }
            let Ok((kind, len)) = parse_record_header(RecordChannel::ProcessInput, header) else {
                return "bad record from the client";
            };
            if !pipe {
                // With `stdin: "null"` any record closes the connection.
                return "a stdin record without stdin";
            }
            if rd.read_exact(&mut payload[..len]).await.is_err() {
                return "client closed";
            }
            let write = async {
                match kind {
                    RecordKind::Stdin => stdin.write(&payload[..len]).await,
                    _ => stdin.close().await,
                }
            };
            // A program that does not read its stdin holds the write; a client that goes
            // away meanwhile still stops it.
            let written = tokio::select! {
                r = write => r,
                () = peer_gone(rd) => return "client closed",
            };
            match written {
                Ok(()) => {}
                // The guest connection failed: the program ended without reading all of its
                // stdin, and its `EXIT` may be on the way. Keep relaying the output; discard
                // what the client still sends.
                Err(SandboxError::Lost(_)) => {
                    discard(rd).await;
                    return "client closed";
                }
                // A record after `STDIN_EOF`.
                Err(_) => return "stdin refused",
            }
        }
    };
    tokio::select! {
        r = to_client => r.unwrap_or("client closed"),
        r = from_client => r,
    }
}

/// Read and drop what the client sends until it closes.
async fn discard(rd: &mut OwnedReadHalf) {
    let mut buf = vec![0u8; MAX_RECORD_DATA_LEN];
    while matches!(rd.read(&mut buf).await, Ok(n) if n > 0) {}
}

async fn write_record(
    wr: &mut OwnedWriteHalf,
    kind: RecordKind,
    data: &[u8],
) -> std::io::Result<()> {
    let rec = encode_record(kind, data).map_err(std::io::Error::other)?;
    wr.write_all(&rec).await?;
    wr.flush().await
}

/// Resolves when the client has closed its end (or the socket failed).
async fn peer_gone(rd: &OwnedReadHalf) {
    loop {
        match rd.ready(Interest::READABLE).await {
            Ok(r) if r.is_read_closed() || r.is_error() => return,
            Ok(_) => tokio::time::sleep(PEER_POLL).await,
            Err(_) => return,
        }
    }
}
