//! Protocol 0.3 connections of the mock `crabd`: scripted `process.open` programs with
//! `process.cancel`, and forwards with a stand-in for the program in the sandbox.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use sandcrab_protocol::{
    FileIdentity, ForwardAcceptAck, ForwardAcceptParams, ForwardListenAck, ForwardListenParams,
    ForwardPortStats, ForwardRejectParams, ForwardStatsResult, FsIdentityResult,
    METHOD_FORWARD_REJECT, ProcessCancelParams, ProcessCancelResult, ProcessExit, ProcessOpenAck,
    ProcessOpenParams, ProtocolError, RECORD_HEADER_LEN, RecordChannel, RecordKind, RpcMessage,
    RpcRequest, RpcResponse, SpawnTarget, StderrMode, encode_frame, encode_record,
    forward::{FORWARD_UNCLAIMED_TIMEOUT_MS, MAX_PENDING_FORWARD_CONNS},
    parse_record_header,
    record::MAX_RECORD_DATA_LEN,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, WriteHalf};
use tokio::sync::{mpsc, oneshot, watch};

use crate::client::{BoxedStream, read_one_message};

/// How long a cancel for an unseen id is remembered (`crabd`'s `EARLY_CANCEL_TTL`).
const EARLY_CANCEL_TTL: Duration = Duration::from_secs(60);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn reply<S: AsyncWrite + Unpin + ?Sized>(stream: &mut S, resp: &RpcResponse) {
    let frame = encode_frame(resp).expect("encode");
    let _ = stream.write_all(&frame).await;
    let _ = stream.flush().await;
}

/// `fs.identity` against the test host's filesystem.
pub(super) fn fs_identity(paths: &[String]) -> Result<FsIdentityResult, ProtocolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        paths
            .iter()
            .map(|p| {
                std::fs::metadata(p)
                    .map(|m| FileIdentity {
                        dev: m.dev(),
                        ino: m.ino(),
                    })
                    .map_err(|e| ProtocolError::NotFound(format!("{p}: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(FsIdentityResult)
    }
    #[cfg(not(unix))]
    {
        let _ = paths;
        Err(ProtocolError::Unimplemented("fs.identity".into()))
    }
}

// ------------------------------------------------------------------------------------------
// Processes
// ------------------------------------------------------------------------------------------

/// Scripted `process.open` programs, chosen by `argv[0]`:
///
/// | `argv` | Behaviour |
/// | --- | --- |
/// | `echo` | Each `STDIN` record comes back as one `STDOUT` record; exits 0 at `STDIN_EOF` |
/// | `exit <code>` | Exits with `<code>`, stderr tail `"bye"` |
/// | `emit <n>` | Writes `n` bytes (`i % 251`) as 64 KiB `STDOUT` records, then exits 0 |
/// | `mixed` | `STDOUT "a"`, `STDERR "b"` (only with `stderr: "stream"`), `STDOUT "c"`, exit 0 |
/// | `env` | One `STDOUT` record: `KEY=value\n` per env entry, sorted; then exit 0 |
/// | `sleep` | Runs until cancelled or its connection closes |
/// | `slow-start <ms>` | Waits `<ms>` before it "starts" (a cancel meanwhile is remembered), then runs like `sleep` |
/// | `hangup` | Acks, then closes the connection without `EXIT` (the host sees it as lost) |
/// | `hangup-before-ack` | Closes the connection without answering |
/// | `limit` | `-32011 LIMIT_EXCEEDED {processes, 64}` |
/// | `no-cwd` | `-32010 SPAWN_FAILED {ENOENT, cwd}` |
/// | anything else | `-32010 SPAWN_FAILED {ENOENT, program}` |
///
/// A cancelled program ends with `EXIT {signal: 15, cancelled: true}`. A program whose
/// connection closes just stops (no `EXIT`), as the real supervisor stops the tree.
/// A cancel for an id the mock has not seen is remembered for 60 s, like `crabd`: an open of
/// that id within the window is answered `CANCELLED`.
#[derive(Debug, Default)]
pub struct MockProcesses {
    table: Mutex<HashMap<String, Slot>>,
    early_cancels: Mutex<HashMap<String, Instant>>,
    /// Programs acked so far.
    pub opened: AtomicU32,
    /// `process.open` frames read so far (acked or not).
    pub open_requests: AtomicU32,
    /// `process.cancel` calls received.
    pub cancels: AtomicU32,
    /// Programs that ended without `EXIT`: their connection closed (or `hangup`).
    pub closed_by_host: AtomicU32,
    /// `STDOUT` payload bytes written to process connections so far.
    pub stdout_written: AtomicU64,
}

#[derive(Debug)]
enum Slot {
    /// The frame was read, the program has not started.
    Pending { cancelled: bool },
    Running {
        cancel: watch::Sender<Option<u64>>,
        done: watch::Receiver<bool>,
    },
}

enum Input {
    Data(Vec<u8>),
    Eof,
}

/// How a script ended.
enum End {
    Exit(ProcessExit),
    /// The connection closed: no `EXIT`.
    Closed,
}

fn exit(code: i32) -> ProcessExit {
    ProcessExit {
        code: Some(code),
        signal: None,
        stderr_tail: String::new(),
        timed_out: false,
        cancelled: false,
        overflow: false,
        tree_clean: true,
        duration_ms: 1,
    }
}

impl MockProcesses {
    /// Whether `process_id` was read and has not ended yet.
    #[must_use]
    pub fn contains(&self, process_id: &str) -> bool {
        lock(&self.table).contains_key(process_id)
    }

    /// Number of programs read and not ended yet.
    #[must_use]
    pub fn live(&self) -> usize {
        lock(&self.table).len()
    }

    /// `process.cancel`, with the remembered-cancel rule.
    pub(super) async fn cancel(&self, p: &ProcessCancelParams) -> ProcessCancelResult {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        let mut done = {
            let mut t = lock(&self.table);
            match t.get_mut(&p.process_id) {
                None => {
                    lock(&self.early_cancels).insert(p.process_id.clone(), Instant::now());
                    return ProcessCancelResult {
                        was_running: false,
                        tree_clean: true,
                    };
                }
                Some(Slot::Pending { cancelled }) => {
                    *cancelled = true;
                    return ProcessCancelResult {
                        was_running: false,
                        tree_clean: true,
                    };
                }
                Some(Slot::Running { cancel, done }) => {
                    let _ = cancel.send(Some(p.effective_grace_ms()));
                    done.clone()
                }
            }
        };
        let _ = done.wait_for(|d| *d).await;
        ProcessCancelResult {
            was_running: true,
            tree_clean: true,
        }
    }

    pub(super) async fn serve_open(&self, mut stream: BoxedStream, req: RpcRequest) {
        self.open_requests.fetch_add(1, Ordering::SeqCst);
        let params = match ProcessOpenParams::from_request(&req) {
            Ok(p) => p,
            Err(e) => return reply(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await,
        };
        let id = params.process_id.clone();
        let early = lock(&self.early_cancels)
            .remove(&id)
            .is_some_and(|at| at.elapsed() < EARLY_CANCEL_TTL);
        let duplicate = {
            let mut t = lock(&self.table);
            let dup = t.contains_key(&id);
            if !dup {
                t.insert(id.clone(), Slot::Pending { cancelled: early });
            }
            dup
        };
        if duplicate {
            let e = ProtocolError::ExecutionFailed(format!("process {id} is already open"));
            return reply(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await;
        }
        let argv: Vec<&str> = params.argv.iter().map(String::as_str).collect();
        let refuse = match argv[0] {
            "hangup-before-ack" => {
                lock(&self.table).remove(&id);
                return;
            }
            "limit" => Some(ProtocolError::LimitExceeded {
                what: "processes".into(),
                limit: 64,
            }),
            "no-cwd" => Some(ProtocolError::SpawnFailed {
                errno: "ENOENT".into(),
                what: SpawnTarget::Cwd,
            }),
            "echo" | "exit" | "emit" | "mixed" | "env" | "sleep" | "slow-start" | "hangup" => None,
            _ => Some(ProtocolError::SpawnFailed {
                errno: "ENOENT".into(),
                what: SpawnTarget::Program,
            }),
        };
        if let Some(e) = refuse {
            lock(&self.table).remove(&id);
            return reply(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await;
        }
        if argv[0] == "slow-start" {
            let ms = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        let (cancel_tx, mut cancel_rx) = watch::channel(None::<u64>);
        let (done_tx, done_rx) = watch::channel(false);
        let cancelled = {
            let mut t = lock(&self.table);
            let c = matches!(t.get(&id), Some(Slot::Pending { cancelled: true }));
            if c {
                t.remove(&id);
            } else {
                t.insert(
                    id.clone(),
                    Slot::Running {
                        cancel: cancel_tx,
                        done: done_rx,
                    },
                );
            }
            c
        };
        if cancelled {
            let e = ProtocolError::Cancelled(format!("process {id} was cancelled"));
            return reply(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await;
        }
        let ack = ProcessOpenAck {
            process_id: id.clone(),
            pid: 4242,
        };
        reply(&mut stream, &RpcResponse::success(req.id, ack.to_value())).await;
        self.opened.fetch_add(1, Ordering::SeqCst);

        let (mut reader, mut writer) = tokio::io::split(stream);
        // stdin, one record at a time (the channel holds one: backpressure).
        let (in_tx, mut in_rx) = mpsc::channel::<Input>(1);
        let input = async move {
            loop {
                let mut header = [0u8; RECORD_HEADER_LEN];
                if reader.read_exact(&mut header).await.is_err() {
                    return;
                }
                let Ok((kind, len)) = parse_record_header(RecordChannel::ProcessInput, header)
                else {
                    return;
                };
                let mut payload = vec![0u8; len];
                if reader.read_exact(&mut payload).await.is_err() {
                    return;
                }
                let item = if kind == RecordKind::Stdin {
                    Input::Data(payload)
                } else {
                    Input::Eof
                };
                if in_tx.send(item).await.is_err() {
                    return;
                }
            }
        };
        let input_task = tokio::spawn(input);

        let script = self.run(&argv, &params, &mut writer, &mut in_rx);
        let end = tokio::select! {
            end = script => end,
            _ = cancel_rx.wait_for(Option::is_some) => End::Exit(ProcessExit {
                code: None,
                signal: Some(15),
                cancelled: true,
                ..exit(0)
            }),
        };
        match end {
            End::Exit(e) => {
                let _ = writer.write_all(&e.encode_record()).await;
                let _ = writer.flush().await;
                let _ = writer.shutdown().await;
            }
            End::Closed => {
                self.closed_by_host.fetch_add(1, Ordering::SeqCst);
            }
        }
        input_task.abort();
        lock(&self.table).remove(&id);
        let _ = done_tx.send(true);
    }

    /// Run the script until it ends or its connection closes.
    async fn run(
        &self,
        argv: &[&str],
        params: &ProcessOpenParams,
        w: &mut WriteHalf<BoxedStream>,
        input: &mut mpsc::Receiver<Input>,
    ) -> End {
        let started = Instant::now();
        let finish = |mut e: ProcessExit| {
            e.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            End::Exit(e)
        };
        match argv[0] {
            "echo" => loop {
                match input.recv().await {
                    Some(Input::Data(d)) => {
                        if self.out(w, RecordKind::Stdout, &d).await.is_err() {
                            return End::Closed;
                        }
                    }
                    Some(Input::Eof) => return finish(exit(0)),
                    None => return End::Closed,
                }
            },
            "exit" => {
                let code = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                finish(ProcessExit {
                    stderr_tail: "bye".into(),
                    ..exit(code)
                })
            }
            "emit" => {
                let n: usize = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
                for chunk in data.chunks(MAX_RECORD_DATA_LEN) {
                    if self.out(w, RecordKind::Stdout, chunk).await.is_err() {
                        return End::Closed;
                    }
                }
                finish(exit(0))
            }
            "mixed" => {
                let mut recs = vec![(RecordKind::Stdout, &b"a"[..])];
                if params.stderr == StderrMode::Stream {
                    recs.push((RecordKind::Stderr, b"b"));
                }
                recs.push((RecordKind::Stdout, b"c"));
                for (k, d) in recs {
                    if self.out(w, k, d).await.is_err() {
                        return End::Closed;
                    }
                }
                let tail = if params.stderr == StderrMode::Merge {
                    ""
                } else {
                    "b"
                };
                finish(ProcessExit {
                    stderr_tail: tail.into(),
                    ..exit(0)
                })
            }
            "env" => {
                let text: String = params
                    .env
                    .iter()
                    .map(|(k, v)| format!("{k}={v}\n"))
                    .collect();
                if self
                    .out(w, RecordKind::Stdout, text.as_bytes())
                    .await
                    .is_err()
                {
                    return End::Closed;
                }
                finish(exit(0))
            }
            "hangup" => End::Closed,
            // `sleep`, `slow-start`: until cancelled (the caller's select) or closed.
            _ => loop {
                if input.recv().await.is_none() {
                    return End::Closed;
                }
            },
        }
    }

    async fn out(
        &self,
        w: &mut WriteHalf<BoxedStream>,
        kind: RecordKind,
        data: &[u8],
    ) -> io::Result<()> {
        let rec = encode_record(kind, data).map_err(io::Error::other)?;
        w.write_all(&rec).await?;
        w.flush().await?;
        if kind == RecordKind::Stdout {
            self.stdout_written
                .fetch_add(data.len() as u64, Ordering::SeqCst);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------
// Forwards
// ------------------------------------------------------------------------------------------

/// The forward side of the mock `crabd`: fixed ports, one `forward.listen` connection at a
/// time, refusals counted as the real guest counts them (`no_listener`, `cap` for more than
/// 32 announced connections, `unclaimed`, `rejected`).
#[derive(Debug)]
pub struct MockForwards {
    ports: Vec<u16>,
    state: Mutex<FwdState>,
    /// How long an announced connection waits for `forward.accept` (ms).
    pub unclaimed_timeout_ms: AtomicU64,
    /// `forward.listen` connections acked so far.
    pub listen_connections: AtomicU32,
}

#[derive(Debug, Default)]
struct FwdState {
    listener: Option<Listener>,
    next_conn_id: u64,
    next_listener: u64,
    pending: HashMap<u64, (u16, oneshot::Sender<Claim>)>,
    stats: BTreeMap<u16, ForwardPortStats>,
}

#[derive(Debug)]
struct Listener {
    id: u64,
    frames: mpsc::Sender<Vec<u8>>,
    kill: Option<oneshot::Sender<()>>,
}

enum Claim {
    Accept(BoxedStream),
    Reject,
}

impl std::fmt::Debug for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Accept(_) => "Accept",
            Self::Reject => "Reject",
        })
    }
}

impl MockForwards {
    #[must_use]
    pub fn new(ports: &[u16]) -> Self {
        Self {
            ports: ports.to_vec(),
            state: Mutex::new(FwdState {
                stats: ports
                    .iter()
                    .map(|&port| {
                        (
                            port,
                            ForwardPortStats {
                                port,
                                ..ForwardPortStats::default()
                            },
                        )
                    })
                    .collect(),
                ..FwdState::default()
            }),
            unclaimed_timeout_ms: AtomicU64::new(FORWARD_UNCLAIMED_TIMEOUT_MS),
            listen_connections: AtomicU32::new(0),
        }
    }

    #[must_use]
    pub fn ports(&self) -> &[u16] {
        &self.ports
    }

    /// `forward.stats`, in port order.
    #[must_use]
    pub fn stats(&self) -> ForwardStatsResult {
        ForwardStatsResult(lock(&self.state).stats.values().copied().collect())
    }

    /// The counters of `port`.
    #[must_use]
    pub fn stats_of(&self, port: u16) -> ForwardPortStats {
        lock(&self.state)
            .stats
            .get(&port)
            .copied()
            .unwrap_or_default()
    }

    /// Whether a `forward.listen` connection is open.
    #[must_use]
    pub fn has_listener(&self) -> bool {
        lock(&self.state)
            .listener
            .as_ref()
            .is_some_and(|l| !l.frames.is_closed())
    }

    /// Close the current `forward.listen` connection from the guest side.
    pub fn drop_listener(&self) {
        if let Some(mut l) = lock(&self.state).listener.take()
            && let Some(kill) = l.kill.take()
        {
            let _ = kill.send(());
        }
    }

    fn count(&self, port: u16, f: impl FnOnce(&mut ForwardPortStats)) {
        if let Some(s) = lock(&self.state).stats.get_mut(&port) {
            f(s);
        }
    }

    /// A program in the sandbox connects to `127.0.0.1:<port>`. A port that is not forwarded
    /// is `ConnectionRefused`. Otherwise the connection is "accepted" at once and handed to
    /// the host as `crabd` does; a refusal closes it (the stream reads end-of-file).
    pub fn connect(self: &std::sync::Arc<Self>, port: u16) -> io::Result<DuplexStream> {
        if !self.ports.contains(&port) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("nothing listens on 127.0.0.1:{port}"),
            ));
        }
        let (program, crabd_end) = tokio::io::duplex(64 * 1024);
        let me = std::sync::Arc::clone(self);
        tokio::spawn(async move { me.announce(port, crabd_end).await });
        Ok(program)
    }

    async fn announce(&self, port: u16, conn: DuplexStream) {
        let (conn_id, rx) = {
            let mut st = lock(&self.state);
            let Some(frames) = st
                .listener
                .as_ref()
                .filter(|l| !l.frames.is_closed())
                .map(|l| l.frames.clone())
            else {
                drop(st);
                self.count(port, |s| s.refused.no_listener += 1);
                return;
            };
            if st.pending.len() >= MAX_PENDING_FORWARD_CONNS as usize {
                drop(st);
                self.count(port, |s| s.refused.cap += 1);
                return;
            }
            st.next_conn_id += 1;
            let conn_id = st.next_conn_id;
            let (tx, rx) = oneshot::channel();
            st.pending.insert(conn_id, (port, tx));
            let frame = encode_frame(
                &sandcrab_protocol::ForwardIncoming { conn_id, port }.to_notification(),
            )
            .expect("encode");
            if frames.try_send(frame).is_err() {
                st.pending.remove(&conn_id);
                drop(st);
                self.count(port, |s| s.refused.no_listener += 1);
                return;
            }
            (conn_id, rx)
        };
        let wait = Duration::from_millis(self.unclaimed_timeout_ms.load(Ordering::SeqCst));
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(Claim::Accept(host))) => {
                // The same relay the host uses: bytes here, records towards the host.
                let _ = crate::forward::relay(conn, host).await;
            }
            Ok(Ok(Claim::Reject)) => self.count(port, |s| s.refused.rejected += 1),
            Ok(Err(_)) => {}
            Err(_) => {
                lock(&self.state).pending.remove(&conn_id);
                self.count(port, |s| s.refused.unclaimed += 1);
            }
        }
    }

    pub(super) async fn serve_listen(&self, mut stream: BoxedStream, req: RpcRequest) {
        if let Err(e) = ForwardListenParams::from_request(&req) {
            return reply(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await;
        }
        let (frames_tx, mut frames_rx) = mpsc::channel::<Vec<u8>>(64);
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let me = {
            let mut st = lock(&self.state);
            if st.listener.as_ref().is_some_and(|l| !l.frames.is_closed()) {
                None
            } else {
                st.next_listener += 1;
                st.listener = Some(Listener {
                    id: st.next_listener,
                    frames: frames_tx,
                    kill: Some(kill_tx),
                });
                Some(st.next_listener)
            }
        };
        let Some(me) = me else {
            let e = ProtocolError::ExecutionFailed(
                "a forward.listen connection is already open".into(),
            );
            return reply(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await;
        };
        let ack = ForwardListenAck {
            ports: self.ports.clone(),
        };
        reply(&mut stream, &RpcResponse::success(req.id, ack.to_value())).await;
        self.listen_connections.fetch_add(1, Ordering::SeqCst);

        let (mut reader, mut writer) = tokio::io::split(stream);
        let write = async {
            while let Some(f) = frames_rx.recv().await {
                if writer.write_all(&f).await.is_err() || writer.flush().await.is_err() {
                    return;
                }
            }
        };
        let read = async {
            loop {
                match read_one_message(&mut reader).await {
                    Ok(RpcMessage::Notification(n)) if n.method == METHOD_FORWARD_REJECT => {
                        let Ok(p) = ForwardRejectParams::from_notification(&n) else {
                            return;
                        };
                        let claim = lock(&self.state).pending.remove(&p.conn_id);
                        if let Some((_, tx)) = claim {
                            let _ = tx.send(Claim::Reject);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        };
        tokio::select! {
            () = write => {}
            () = read => {}
            _ = kill_rx => {}
        }
        let mut st = lock(&self.state);
        // Announced connections survive the listen connection until their unclaimed
        // deadline (as in `crabd`): a host that listens again may still accept them.
        if st.listener.as_ref().is_none_or(|l| l.id == me) {
            st.listener = None;
        }
    }

    pub(super) async fn serve_accept(&self, mut stream: BoxedStream, req: RpcRequest) {
        let p = match ForwardAcceptParams::from_request(&req) {
            Ok(p) => p,
            Err(e) => return reply(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await,
        };
        let claim = lock(&self.state).pending.remove(&p.conn_id);
        let Some((port, tx)) = claim else {
            let e = ProtocolError::NotFound(format!("no announced connection {}", p.conn_id));
            return reply(&mut stream, &RpcResponse::failure(Some(req.id), &e)).await;
        };
        reply(
            &mut stream,
            &RpcResponse::success(req.id, ForwardAcceptAck { port }.to_value()),
        )
        .await;
        self.count(port, |s| s.accepted += 1);
        let _ = tx.send(Claim::Accept(stream));
    }
}
