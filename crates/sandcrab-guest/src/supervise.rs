//! `process.open`: one program per connection, run by a short-lived `crabd supervise`.
//!
//! # The two halves
//!
//! **`crabd serve`** ([`serve_open`], [`ProcessTable`]) reads the `process.open` frame,
//! validates it, registers the `process_id` (so a `process.cancel` can never lose a race with
//! the open), and starts `crabd supervise` with:
//!
//! - **stdin**: the read end of a *control pipe*. `crabd serve` writes the spec (the request id
//!   and the params, env included) as one frame, then keeps the pipe for cancel bytes
//!   ([`CTRL_ASK`], [`CTRL_FORCE`]). The spec never appears on a command line, and a cancel
//!   is ordered after the spec without any signal race. EOF on the pipe (`crabd serve` stops
//!   or dies) stops the tree.
//! - **stdout**: the connection itself (a duplicate of the socket, Unix, TCP or vsock).
//!   `crabd serve` drops its own copy right after the start and is not in the data path.
//! - **stderr**: inherited, so the supervisor's logs go where `crabd serve`'s go.
//!
//! The descriptor `crabd serve` hands over is non-blocking (Tokio set it, and the flag lives
//! in the open file description, shared with the copy). The supervisor keeps it that way and
//! drives it with its own (single-thread) Tokio runtime. `crabd serve`
//! never reads, writes or shuts down its copy after the handover; it only closes it.
//!
//! **`crabd supervise`** ([`main`]) reads the spec, marks itself not dumpable, becomes a child
//! subreaper, changes into the cwd, starts the program (cleared environment plus exactly the
//! given one, the given argv, no shell, its own process group), answers the `process.open`
//! request on the connection (ack or `SPAWN_FAILED`), then pumps records until the tree is
//! gone, writes `EXIT` after the last output byte, and exits: 0 when the tree is clean,
//! [`EXIT_UNCLEAN`] when something could not be stopped, [`EXIT_ERROR`] on an internal error.
//! `crabd serve` reads `tree_clean` from that status.
//!
//! # Stopping the tree
//!
//! The tree is every process whose parent chain leads to the supervisor
//! ([`crate::proctree`]). *Ask* is `SIGTERM` to the leader's group and to every process of the
//! tree; *force* is `SIGKILL` to all of them, reap, list again, until two listings in a row are
//! empty, for at most [`FORCE_LOOP_MS`]. When the leader exits, whatever it left behind is
//! forced at once; a closed connection or control pipe is ask, `kill_grace_ms`, force.
//!
//! No parent-death signal is used: it follows the thread that started the child, which is
//! wrong under a multi-thread runtime. The closed connection covers a dead host.
//!
//! # What this step honours
//!
//! `process_id`, `argv`, `env`, `cwd`, `stdin`, `kill_grace_ms`. `stderr` is always sent as
//! `STDERR` records (the `stream` behaviour) and `stderr_tail` is empty, whatever the mode.
//! `stderr_tail_bytes`, `limits`, `oom_score_adj`, `timeout_ms` and `output_cap` are accepted
//! and not applied yet: they are the `guest-caps` step (see the `guest-caps` markers below).

use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read},
    os::{
        fd::{AsFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
    process::{ExitCode, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use rustix::{
    io::Errno,
    process::{Pid, Signal, WaitOptions, WaitStatus},
};
use sandcrab_protocol::{
    HEADER_LEN, MAX_FRAME_LEN, ProcessCancelParams, ProcessCancelResult, ProcessExit,
    ProcessOpenAck, ProcessOpenParams, ProtocolError, RECORD_HEADER_LEN, RecordChannel, RecordKind,
    RequestId, RpcRequest, RpcResponse, SpawnTarget, StdinMode, encode_frame, encode_record_header,
    parse_header, parse_record_header,
    process::{DEFAULT_MAX_PROCESSES, DRAIN_AFTER_EXIT_MS, FORCE_LOOP_MS},
    record::MAX_RECORD_DATA_LEN,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Interest},
    net::unix::pipe,
    sync::{OwnedMutexGuard, mpsc, watch},
};
use tracing::{debug, error, info, warn};

use crate::{ToolContext, codec};

/// Control byte: ask the tree to stop (`SIGTERM`).
pub const CTRL_ASK: u8 = b'A';
/// Control byte: force the tree to stop (`SIGKILL` loop).
pub const CTRL_FORCE: u8 = b'F';
/// Supervisor exit status when some process of the tree could not be stopped.
pub const EXIT_UNCLEAN: u8 = 3;
/// Supervisor exit status on an internal error (the tree's state is unknown).
pub const EXIT_ERROR: u8 = 2;

/// A cancel for an id `crabd serve` has not seen yet is remembered this long...
const EARLY_CANCEL_TTL: Duration = Duration::from_secs(60);
/// ...and at most this many of them.
const MAX_EARLY_CANCELS: usize = 1024;
/// How often the tree is listed while waiting for it to empty.
const LIST_INTERVAL: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------------------------
// `crabd serve` side
// ---------------------------------------------------------------------------------------------

/// How `crabd serve` starts supervisors.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// The `crabd` binary run as `crabd supervise`. Default: this executable.
    pub exe: PathBuf,
    /// Processes open at once (`--max-processes`).
    pub max_processes: usize,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            exe: default_exe(),
            max_processes: DEFAULT_MAX_PROCESSES as usize,
        }
    }
}

fn default_exe() -> PathBuf {
    // `/proc/self/exe` names this very binary, even if its path was replaced since.
    if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("crabd"))
    }
}

/// What `crabd serve` hands a supervisor through the control pipe, as one frame.
#[derive(Debug, Serialize, Deserialize)]
struct Spec {
    request_id: RequestId,
    /// Validated, with `cwd` resolved to the sandbox's default when it was omitted.
    params: ProcessOpenParams,
}

/// The `process_id`s of `process.open` programs, from the moment their frame was read until
/// their supervisor exited. Held by [`ToolContext::supervisors`].
#[derive(Default)]
pub struct ProcessTable {
    inner: Mutex<TableInner>,
}

impl std::fmt::Debug for ProcessTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessTable")
            .field("open", &lock(&self.inner).entries.len())
            .finish()
    }
}

#[derive(Default)]
struct TableInner {
    entries: HashMap<String, Entry>,
    /// Cancels for ids not (yet) in `entries`: an open that arrives later is `CANCELLED`.
    early_cancels: VecDeque<(String, Instant)>,
}

enum Entry {
    /// Frame read, supervisor not started. `cancelled`: a cancel arrived meanwhile.
    Pending {
        cancelled: bool,
    },
    Running(Arc<Running>),
}

struct Running {
    /// The control pipe; `None` once closed.
    control: Arc<tokio::sync::Mutex<Option<pipe::Sender>>>,
    /// `Some(tree_clean)` once the supervisor exited.
    done: watch::Receiver<Option<bool>>,
}

impl Running {
    async fn send(&self, byte: u8) {
        let mut control = self.control.lock().await;
        if let Some(tx) = control.as_mut()
            && tx.write_all(&[byte]).await.is_err()
        {
            *control = None;
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl TableInner {
    fn prune_early_cancels(&mut self, now: Instant) {
        while self
            .early_cancels
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) > EARLY_CANCEL_TTL)
        {
            self.early_cancels.pop_front();
        }
    }
}

impl ProcessTable {
    /// Register `id` as pending. Fails if it was cancelled already, is in use, or the table
    /// is full.
    fn begin(&self, id: &str, max: usize) -> Result<(), ProtocolError> {
        let mut t = lock(&self.inner);
        t.prune_early_cancels(Instant::now());
        if let Some(i) = t.early_cancels.iter().position(|(x, _)| x == id) {
            t.early_cancels.remove(i);
            return Err(cancelled_error());
        }
        if t.entries.contains_key(id) {
            return Err(ProtocolError::InvalidParams(format!(
                "`process_id` {id} is already in use"
            )));
        }
        if t.entries.len() >= max {
            return Err(ProtocolError::LimitExceeded {
                what: "processes".into(),
                limit: max as u64,
            });
        }
        t.entries
            .insert(id.to_owned(), Entry::Pending { cancelled: false });
        Ok(())
    }

    /// Run `spawn` for the pending `id` unless a cancel arrived; on success the entry becomes
    /// running with its control pipe locked by the returned guard (the caller writes the spec
    /// before any cancel byte can go through). On any failure the entry is removed.
    fn start(
        &self,
        id: &str,
        control: pipe::Sender,
        spawn: impl FnOnce() -> io::Result<tokio::process::Child>,
    ) -> Result<Started, ProtocolError> {
        let mut t = lock(&self.inner);
        if matches!(t.entries.get(id), Some(Entry::Pending { cancelled: true })) {
            t.entries.remove(id);
            return Err(cancelled_error());
        }
        let child = match spawn() {
            Ok(c) => c,
            Err(e) => {
                t.entries.remove(id);
                return Err(ProtocolError::Internal(format!(
                    "could not start the supervisor: {e}"
                )));
            }
        };
        let control = Arc::new(tokio::sync::Mutex::new(Some(control)));
        let guard = Arc::clone(&control)
            .try_lock_owned()
            .expect("a new mutex is unlocked");
        let (done_tx, done) = watch::channel(None);
        t.entries.insert(
            id.to_owned(),
            Entry::Running(Arc::new(Running { control, done })),
        );
        Ok(Started {
            child,
            control: guard,
            done: done_tx,
        })
    }

    /// Drop a pending entry (an error before the start).
    fn abandon(&self, id: &str) {
        let mut t = lock(&self.inner);
        if matches!(t.entries.get(id), Some(Entry::Pending { .. })) {
            t.entries.remove(id);
        }
    }

    fn finish(&self, id: &str) {
        lock(&self.inner).entries.remove(id);
    }

    /// Number of `process.open` programs registered (pending or running).
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.inner).entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `process.cancel`: ask, wait `grace_ms`, force; answer when the supervisor has exited.
    pub async fn cancel(&self, p: &ProcessCancelParams) -> ProcessCancelResult {
        let not_running = ProcessCancelResult {
            was_running: false,
            tree_clean: true,
        };
        let run = {
            let mut t = lock(&self.inner);
            match t.entries.get_mut(&p.process_id) {
                Some(Entry::Pending { cancelled }) => {
                    *cancelled = true;
                    return not_running;
                }
                Some(Entry::Running(r)) => Arc::clone(r),
                None => {
                    // Unknown: the open may still be on its way. Remember the cancel.
                    t.prune_early_cancels(Instant::now());
                    if t.early_cancels.len() >= MAX_EARLY_CANCELS {
                        t.early_cancels.pop_front();
                    }
                    t.early_cancels
                        .push_back((p.process_id.clone(), Instant::now()));
                    return not_running;
                }
            }
        };
        if run.done.borrow().is_some() {
            return not_running;
        }
        let mut done = run.done.clone();
        let grace = Duration::from_millis(p.effective_grace_ms());
        if !grace.is_zero() {
            run.send(CTRL_ASK).await;
            if let Ok(Ok(clean)) = tokio::time::timeout(grace, done.wait_for(Option::is_some)).await
            {
                return ProcessCancelResult {
                    was_running: true,
                    tree_clean: clean.unwrap_or(false),
                };
            }
        }
        run.send(CTRL_FORCE).await;
        let clean = done
            .wait_for(Option::is_some)
            .await
            .map(|c| c.unwrap_or(false))
            .unwrap_or(false);
        ProcessCancelResult {
            was_running: true,
            tree_clean: clean,
        }
    }

    /// Stop every running supervisor (close its control pipe: ask, its `kill_grace_ms`,
    /// force) and wait for them, at most `wait`.
    pub async fn shutdown(&self, wait: Duration) {
        let running: Vec<Arc<Running>> = lock(&self.inner)
            .entries
            .values()
            .filter_map(|e| match e {
                Entry::Running(r) => Some(Arc::clone(r)),
                Entry::Pending { .. } => None,
            })
            .collect();
        if running.is_empty() {
            return;
        }
        info!(count = running.len(), "stopping process.open programs");
        for r in &running {
            *r.control.lock().await = None;
        }
        let all = async {
            for r in &running {
                let _ = r.done.clone().wait_for(Option::is_some).await;
            }
        };
        if tokio::time::timeout(wait, all).await.is_err() {
            warn!("process.open programs still running after shutdown wait");
        }
    }
}

struct Started {
    child: tokio::process::Child,
    control: OwnedMutexGuard<Option<pipe::Sender>>,
    done: watch::Sender<Option<bool>>,
}

fn cancelled_error() -> ProtocolError {
    ProtocolError::Cancelled("`process.cancel` arrived before the program started".into())
}

/// Serve a connection whose first frame was `process.open`. On success the connection
/// belongs to the supervisor; this copy is only closed. On failure the error frame is
/// written here and the connection closed.
pub async fn serve_open<S>(
    mut stream: S,
    req: RpcRequest,
    ctx: Arc<ToolContext>,
    cfg: &SupervisorConfig,
    peer: &str,
) where
    S: AsyncRead + AsyncWrite + AsFd + Unpin,
{
    let id = req.id.clone();
    match hand_over(&stream, req, ctx, cfg).await {
        // Never shut the stream down here: the socket is shared with the supervisor.
        Ok(()) => debug!(%peer, "process connection handed to its supervisor"),
        Err(e) => {
            debug!(%peer, error = %e, "process.open refused");
            let _ = codec::write_frame(&mut stream, &RpcResponse::failure(Some(id), &e)).await;
        }
    }
}

async fn hand_over<S: AsFd>(
    stream: &S,
    req: RpcRequest,
    ctx: Arc<ToolContext>,
    cfg: &SupervisorConfig,
) -> Result<(), ProtocolError> {
    let mut params = ProcessOpenParams::from_request(&req)?;
    let pid_key = params.process_id.clone();
    ctx.supervisors.begin(&pid_key, cfg.max_processes)?;
    let prepared = (|| -> Result<_, ProtocolError> {
        if params.cwd.is_none() {
            params.cwd = Some(ctx.default_cwd.to_string_lossy().into_owned());
        }
        let spec = encode_frame(&Spec {
            request_id: req.id.clone(),
            params,
        })
        .map_err(|e| ProtocolError::InvalidParams(format!("`process.open` too large: {e}")))?;
        let conn = stream.as_fd().try_clone_to_owned().map_err(internal)?;
        let (rd, wr) = std::io::pipe().map_err(internal)?;
        let control = pipe::Sender::from_owned_fd(OwnedFd::from(wr)).map_err(internal)?;
        let mut cmd = tokio::process::Command::new(&cfg.exe);
        cmd.arg("supervise")
            .stdin(Stdio::from(rd))
            .stdout(Stdio::from(conn))
            .stderr(Stdio::inherit())
            .kill_on_drop(false);
        Ok((spec, control, cmd))
    })();
    let (spec, control, mut cmd) = match prepared {
        Ok(v) => v,
        Err(e) => {
            ctx.supervisors.abandon(&pid_key);
            return Err(e);
        }
    };
    let Started {
        mut child,
        control: mut guard,
        done,
    } = ctx.supervisors.start(&pid_key, control, || cmd.spawn())?;
    // The child holds its own copies of the pipe end and the connection now; `cmd` holds the
    // parent's copies until dropped.
    drop(cmd);
    let sup_pid = child.id();
    debug!(process_id = %pid_key, supervisor = ?sup_pid, "supervisor started");
    if let Some(tx) = guard.as_mut()
        && let Err(e) = tx.write_all(&spec).await
    {
        // The supervisor died before reading its spec; its exit is reported below.
        warn!(process_id = %pid_key, error = %e, "could not hand the spec to the supervisor");
        *guard = None;
    }
    drop(guard);
    tokio::spawn(async move {
        let status = child.wait().await;
        let clean = matches!(&status, Ok(s) if s.code() == Some(0));
        if !clean {
            warn!(process_id = %pid_key, status = ?status, "supervisor ended without a clean tree");
        }
        ctx.supervisors.finish(&pid_key);
        let _ = done.send(Some(clean));
    });
    Ok(())
}

fn internal(e: io::Error) -> ProtocolError {
    ProtocolError::Internal(format!("process.open: {e}"))
}

// ---------------------------------------------------------------------------------------------
// `crabd supervise` side
// ---------------------------------------------------------------------------------------------

/// Entry point of `crabd supervise`. Not for humans: `crabd serve` starts it with the
/// control pipe on stdin and the connection on stdout.
pub fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    // One thread: `Command::spawn` reaps a child whose exec failed by itself, so no reaper
    // may run concurrently with it.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "crabd supervise: runtime");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    match rt.block_on(supervise()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(EXIT_UNCLEAN),
        Err(e) => {
            error!(error = %e, "crabd supervise failed");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Events for the supervisor's main loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Ask,
    Force,
    /// `crabd serve` closed the control pipe.
    ControlClosed,
    /// The host closed the connection (or sent something invalid).
    ConnClosed,
}

/// Returns whether the tree ended clean.
async fn supervise() -> io::Result<bool> {
    let control = take_stdin()?;
    let conn = take_stdout()?;
    harden()?;
    let (spec, control) = read_spec(control)?;
    let Spec { request_id, params } = spec;
    let conn = Arc::new(Conn::new(conn)?);

    // Before the spawn, so no SIGCHLD of the leader is missed.
    let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    let started = Instant::now();
    let mut child = match start_program(&params) {
        Ok(c) => c,
        Err(e) => {
            info!(process_id = %params.process_id, error = %e, "process.open: could not start");
            let frame = encode_frame(&RpcResponse::failure(Some(request_id), &e))
                .map_err(io::Error::other)?;
            let _ = conn.write_all(&frame).await;
            return Ok(true);
        }
    };
    let leader = i32::try_from(child.id())
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other("invalid child pid"))?;
    let mut tree = Tree::new(leader);
    let grace = Duration::from_millis(params.effective_kill_grace_ms());
    debug!(process_id = %params.process_id, pid = leader.as_raw_nonzero().get(), "program started");

    let (events_tx, mut events) = mpsc::channel::<Event>(16);
    let (stop_reading, stop_rx) = watch::channel(false);
    let stdin = match child.stdin.take() {
        Some(s) => Some(pipe::Sender::from_owned_fd(OwnedFd::from(s))?),
        None => None,
    };
    let stdout = child.stdout.take().map(OwnedFd::from);
    let stderr = child.stderr.take().map(OwnedFd::from);
    // Dropping a std `Child` neither waits nor kills: the reaper collects the leader.
    drop(child);

    let ack = RpcResponse::success(
        request_id,
        ProcessOpenAck {
            process_id: params.process_id.clone(),
            pid: leader.as_raw_nonzero().get().unsigned_abs(),
        }
        .to_value(),
    );
    let ack = encode_frame(&ack).map_err(io::Error::other)?;
    if conn.write_all(&ack).await.is_err() {
        let clean = tree.stop(grace, &mut events).await;
        return Ok(clean);
    }

    // guest-caps: stderr modes (tail, merge), the output cap and the timeout hook in here.
    let mut pumps = Vec::new();
    for (fd, kind) in [(stdout, RecordKind::Stdout), (stderr, RecordKind::Stderr)] {
        if let Some(fd) = fd {
            let rx = pipe::Receiver::from_owned_fd(fd)?;
            pumps.push(tokio::spawn(pump(
                rx,
                kind,
                Arc::clone(&conn),
                stop_rx.clone(),
                events_tx.clone(),
            )));
        }
    }
    tokio::spawn(read_input(Arc::clone(&conn), stdin, events_tx.clone()));
    tokio::spawn(read_control(control, events_tx));

    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut cancelled = false;
    let mut conn_closed = false;
    let clean = loop {
        tokio::select! {
            _ = sigchld.recv() => {}
            _ = tick.tick() => {}
            ev = events.recv() => match ev {
                Some(Event::Ask) => {
                    cancelled = true;
                    tree.ask();
                }
                Some(Event::Force) => {
                    cancelled = true;
                    break tree.force().await;
                }
                Some(Event::ConnClosed) => {
                    conn_closed = true;
                    break tree.stop(grace, &mut events).await;
                }
                Some(Event::ControlClosed) | None => {
                    cancelled = true;
                    break tree.stop(grace, &mut events).await;
                }
            },
        }
        tree.reap();
        if tree.leader_status.is_some() {
            // The tree dies with its leader.
            break tree.force().await;
        }
    };
    if conn_closed {
        return Ok(clean);
    }

    // Exit after the last byte: wait for both pipes to reach EOF. A process outside the tree
    // that still holds a pipe cannot delay the exit beyond the drain bound.
    let drain = Duration::from_millis(DRAIN_AFTER_EXIT_MS);
    let mut all_pumps = std::pin::pin!(async {
        for p in pumps {
            let _ = p.await;
        }
    });
    if tokio::time::timeout(drain, &mut all_pumps).await.is_err() {
        let _ = stop_reading.send(true);
        all_pumps.await;
    }
    let status = tree.leader_status;
    let exit = ProcessExit {
        code: status.and_then(WaitStatus::exit_status),
        signal: status.and_then(WaitStatus::terminating_signal),
        stderr_tail: String::new(),
        timed_out: false,
        cancelled,
        overflow: false,
        tree_clean: clean,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    let exit = exit.encode_record();
    let write = conn.write_all(&exit);
    // After a cancel nobody may be reading: do not let the answer to the cancel hang on it.
    let written = if cancelled {
        tokio::time::timeout(drain, write)
            .await
            .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
    } else {
        write.await
    };
    if let Err(e) = written {
        debug!(error = %e, "EXIT not delivered");
    }
    Ok(clean)
}

/// Take stdin (the control pipe) and put `/dev/null` in its place.
fn take_stdin() -> io::Result<OwnedFd> {
    let fd = io::stdin().as_fd().try_clone_to_owned()?;
    rustix::stdio::dup2_stdin(std::fs::File::open("/dev/null")?)?;
    Ok(fd)
}

/// Take stdout (the connection) and put `/dev/null` in its place, so nothing printed by
/// accident can corrupt the record stream.
fn take_stdout() -> io::Result<OwnedFd> {
    let fd = io::stdout().as_fd().try_clone_to_owned()?;
    rustix::stdio::dup2_stdout(std::fs::OpenOptions::new().write(true).open("/dev/null")?)?;
    Ok(fd)
}

/// Not dumpable (the program, same uid, cannot read the spec from our memory) and a child
/// subreaper (every orphaned descendant is re-parented to us, so the tree stays findable).
fn harden() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
        rustix::process::set_child_subreaper(Some(rustix::process::getpid()))?;
    }
    Ok(())
}

/// Read the spec frame from the control pipe (blocking: nothing else runs yet), then hand the
/// pipe back for the cancel bytes.
fn read_spec(control: OwnedFd) -> io::Result<(Spec, pipe::Receiver)> {
    let mut f = std::fs::File::from(control);
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header)?;
    let len = parse_header(header).map_err(io::Error::other)?;
    debug_assert!(len <= MAX_FRAME_LEN);
    let mut payload = vec![0u8; len];
    f.read_exact(&mut payload)?;
    let spec: Spec = serde_json::from_slice(&payload).map_err(io::Error::other)?;
    let control = pipe::Receiver::from_owned_fd(OwnedFd::from(f))?;
    Ok((spec, control))
}

/// Change into the cwd and start the program: exact env, exact argv, no shell, its own
/// process group. A failure is `SPAWN_FAILED` naming the cwd or the program.
fn start_program(p: &ProcessOpenParams) -> Result<std::process::Child, ProtocolError> {
    if let Some(cwd) = &p.cwd {
        // Our own cwd: the supervisor runs nothing else, and the error is exact (a `chdir`
        // in the child would report the same errno as a missing program).
        std::env::set_current_dir(cwd).map_err(|e| spawn_failed(&e, SpawnTarget::Cwd))?;
    }
    // guest-caps: rlimits and oom_score_adj are applied to ourselves here, before the spawn.
    let mut cmd = std::process::Command::new(&p.argv[0]);
    cmd.args(&p.argv[1..])
        .env_clear()
        .envs(&p.env)
        .process_group(0)
        .stdin(match p.stdin {
            StdinMode::Null => Stdio::null(),
            StdinMode::Pipe => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
        .map_err(|e| spawn_failed(&e, SpawnTarget::Program))
}

fn spawn_failed(e: &io::Error, what: SpawnTarget) -> ProtocolError {
    ProtocolError::SpawnFailed {
        errno: errno_name(e),
        what,
    }
}

/// The symbolic name of an error's errno (`ENOENT`, ...).
#[must_use]
pub fn errno_name(e: &io::Error) -> String {
    let Some(raw) = e.raw_os_error() else {
        return "EIO".into();
    };
    let name = match Errno::from_raw_os_error(raw) {
        Errno::NOENT => "ENOENT",
        Errno::ACCESS => "EACCES",
        Errno::NOEXEC => "ENOEXEC",
        Errno::NOTDIR => "ENOTDIR",
        Errno::ISDIR => "EISDIR",
        Errno::LOOP => "ELOOP",
        Errno::NAMETOOLONG => "ENAMETOOLONG",
        Errno::TOOBIG => "E2BIG",
        Errno::TXTBSY => "ETXTBSY",
        Errno::PERM => "EPERM",
        Errno::NOMEM => "ENOMEM",
        Errno::AGAIN => "EAGAIN",
        Errno::MFILE => "EMFILE",
        Errno::NFILE => "ENFILE",
        Errno::INVAL => "EINVAL",
        Errno::IO => "EIO",
        _ => return format!("errno {raw}"),
    };
    name.into()
}

/// The connection, non-blocking, driven by this process's runtime.
///
/// Held as a Tokio `UnixStream` whatever the socket family (Unix, TCP or vsock): only
/// `recv`, `send` and readiness are used, which every stream socket supports. Taking it this
/// way needs no `unsafe` (`AsyncFd`'s constructors now are). It is never shut down: closing
/// it is the only end.
struct Conn {
    stream: tokio::net::UnixStream,
    /// One record at a time.
    write_lock: tokio::sync::Mutex<()>,
}

impl Conn {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        // Already non-blocking when Tokio owned it in `crabd serve` (the flag is shared);
        // set it anyway so the supervisor does not depend on that.
        rustix::io::ioctl_fionbio(&fd, true)?;
        let std = std::os::unix::net::UnixStream::from(fd);
        Ok(Self {
            stream: tokio::net::UnixStream::from_std(std)?,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.stream.readable().await?;
            match self.stream.try_read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                r => return r,
            }
        }
    }

    /// Fill `buf`. `Ok(false)`: EOF before the first byte.
    async fn read_exact(&self, buf: &mut [u8]) -> io::Result<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.read(&mut buf[filled..]).await? {
                0 if filled == 0 => return Ok(false),
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => filled += n,
            }
        }
        Ok(true)
    }

    async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        let _one = self.write_lock.lock().await;
        while !buf.is_empty() {
            self.stream.writable().await?;
            match self.stream.try_write(buf) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => buf = &buf[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn send_record(&self, kind: RecordKind, payload: &[u8]) -> io::Result<()> {
        let header = encode_record_header(kind, payload.len()).map_err(io::Error::other)?;
        let mut rec = Vec::with_capacity(RECORD_HEADER_LEN + payload.len());
        rec.extend_from_slice(&header);
        rec.extend_from_slice(payload);
        self.write_all(&rec).await
    }

    /// Resolves when the peer closed its side, without consuming data. While a record is
    /// waiting to be read the socket stays readable, so it looks again every 100 ms.
    async fn closed(&self) {
        loop {
            match self.stream.ready(Interest::READABLE).await {
                Ok(r) if r.is_read_closed() => return,
                Ok(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(_) => return,
            }
        }
    }
}

/// Read one output pipe; each read becomes one record. The next read happens only after the
/// record was written (backpressure).
async fn pump(
    mut rx: pipe::Receiver,
    kind: RecordKind,
    conn: Arc<Conn>,
    mut stop: watch::Receiver<bool>,
    events: mpsc::Sender<Event>,
) {
    let mut buf = vec![0u8; MAX_RECORD_DATA_LEN];
    loop {
        // `biased`: data already in the pipe is read even after `stop`.
        let n = tokio::select! {
            biased;
            r = rx.read(&mut buf) => r,
            _ = stop.wait_for(|s| *s) => break,
        };
        match n {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if conn.send_record(kind, &buf[..n]).await.is_err() {
                    let _ = events.send(Event::ConnClosed).await;
                    break;
                }
            }
        }
    }
}

/// Read `STDIN` / `STDIN_EOF` records from the connection. Any end of the connection (EOF,
/// error, an invalid record) is [`Event::ConnClosed`].
async fn read_input(conn: Arc<Conn>, mut stdin: Option<pipe::Sender>, events: mpsc::Sender<Event>) {
    let why: String = loop {
        let mut header = [0u8; RECORD_HEADER_LEN];
        match conn.read_exact(&mut header).await {
            Ok(true) => {}
            Ok(false) => break "connection closed".into(),
            Err(e) => break e.to_string(),
        }
        let (kind, len) = match parse_record_header(RecordChannel::ProcessInput, header) {
            Ok(v) => v,
            Err(e) => break e.to_string(),
        };
        let mut payload = vec![0u8; len];
        if let Err(e) = conn.read_exact(&mut payload).await.and_then(|full| {
            if full || len == 0 {
                Ok(())
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            }
        }) {
            break e.to_string();
        }
        match kind {
            RecordKind::Stdin => {
                // No stdin (mode `null`, after `STDIN_EOF`, or the program closed it): drop.
                if let Some(w) = stdin.as_mut() {
                    tokio::select! {
                        r = w.write_all(&payload) => if r.is_err() { stdin = None },
                        () = conn.closed() => break "connection closed while stdin was full".into(),
                    }
                }
            }
            RecordKind::StdinEof => stdin = None,
            _ => unreachable!("parse_record_header checks the channel"),
        }
    };
    debug!(reason = %why, "process connection input ended");
    let _ = events.send(Event::ConnClosed).await;
}

async fn read_control(mut rx: pipe::Receiver, events: mpsc::Sender<Event>) {
    let mut b = [0u8; 1];
    loop {
        let ev = match rx.read(&mut b).await {
            Ok(0) | Err(_) => {
                let _ = events.send(Event::ControlClosed).await;
                return;
            }
            Ok(_) => match b[0] {
                CTRL_ASK => Event::Ask,
                CTRL_FORCE => Event::Force,
                _ => continue,
            },
        };
        if events.send(ev).await.is_err() {
            return;
        }
    }
}

/// The program's tree, from the supervisor's side.
struct Tree {
    me: u32,
    leader: Pid,
    leader_status: Option<WaitStatus>,
    #[cfg(target_os = "linux")]
    procfs: crate::proctree::ProcFs,
}

impl Tree {
    fn new(leader: Pid) -> Self {
        Self {
            me: std::process::id(),
            leader,
            leader_status: None,
            #[cfg(target_os = "linux")]
            procfs: crate::proctree::ProcFs::default(),
        }
    }

    /// Collect every exited child (the leader and adopted orphans): `waitpid(-1, WNOHANG)`.
    /// Safe here: the supervisor starts exactly one child and waits on nothing else.
    fn reap(&mut self) {
        loop {
            match rustix::process::wait(WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => {
                    if pid == self.leader {
                        self.leader_status = Some(status);
                    }
                }
                Ok(None) | Err(Errno::CHILD) => break,
                Err(Errno::INTR) => {}
                Err(e) => {
                    warn!(error = %e, "waitpid(-1)");
                    break;
                }
            }
        }
    }

    /// Live processes of the tree. `Err`: the tree could not be listed.
    fn alive(&self) -> io::Result<Vec<Pid>> {
        #[cfg(target_os = "linux")]
        {
            Ok(self
                .procfs
                .descendants(self.me)?
                .into_iter()
                .filter(|p| !p.is_dead())
                .filter_map(|p| Pid::from_raw(i32::try_from(p.pid).ok()?))
                .collect())
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Fallback (`tree_kill:process_group`): only the leader's group is visible.
            let _ = self.me;
            Ok(
                match rustix::process::test_kill_process_group(self.leader) {
                    Ok(()) => vec![self.leader],
                    Err(_) => Vec::new(),
                },
            )
        }
    }

    /// Send `sig` to the leader's group and to every live process of the tree. False when a
    /// process could not be signalled (or the tree could not be listed).
    fn signal_all(&self, sig: Signal) -> bool {
        let _ = rustix::process::kill_process_group(self.leader, sig);
        let Ok(alive) = self.alive() else {
            return false;
        };
        let mut ok = true;
        for pid in alive {
            match rustix::process::kill_process(pid, sig) {
                Ok(()) | Err(Errno::SRCH) => {}
                Err(e) => {
                    warn!(pid = pid.as_raw_nonzero().get(), error = %e, "cannot signal");
                    ok = false;
                }
            }
        }
        ok
    }

    fn ask(&mut self) {
        self.reap();
        self.signal_all(Signal::TERM);
    }

    /// `SIGKILL` everything, reap, list again, until two listings in a row are empty or
    /// [`FORCE_LOOP_MS`] passed. True: the tree is gone and every signal was delivered.
    async fn force(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(FORCE_LOOP_MS);
        let mut delivered = true;
        let mut empty_in_a_row = 0;
        loop {
            self.reap();
            match self.alive() {
                Ok(alive) if alive.is_empty() => {
                    empty_in_a_row += 1;
                    if empty_in_a_row >= 2 {
                        self.reap();
                        return delivered;
                    }
                }
                Ok(_) => {
                    empty_in_a_row = 0;
                    delivered &= self.signal_all(Signal::KILL);
                }
                Err(e) => {
                    warn!(error = %e, "cannot list the process tree");
                    delivered = false;
                    let _ = rustix::process::kill_process_group(self.leader, Signal::KILL);
                }
            }
            if Instant::now() >= deadline {
                warn!("process tree not empty after the force loop");
                return false;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Ask, wait up to `grace` for the tree to empty (or a force request), then force.
    async fn stop(&mut self, grace: Duration, events: &mut mpsc::Receiver<Event>) -> bool {
        self.ask();
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            self.reap();
            if self.alive().is_ok_and(|a| a.is_empty()) {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => break,
                () = tokio::time::sleep(LIST_INTERVAL) => {}
                ev = events.recv() => if matches!(ev, Some(Event::Force) | None) { break },
            }
        }
        self.force().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "6f1c2a43-8a51-4c43-9d3e-2b0a8e9c1f00";

    #[test]
    fn errno_names() {
        let n = |e: Errno| errno_name(&io::Error::from_raw_os_error(e.raw_os_error()));
        assert_eq!(n(Errno::NOENT), "ENOENT");
        assert_eq!(n(Errno::ACCESS), "EACCES");
        assert_eq!(n(Errno::NOTDIR), "ENOTDIR");
        assert_eq!(n(Errno::NOEXEC), "ENOEXEC");
        assert!(n(Errno::XDEV).starts_with("errno "));
        assert_eq!(errno_name(&io::Error::other("x")), "EIO");
    }

    #[test]
    fn table_begin_rejects_duplicates_and_full() {
        let t = ProcessTable::default();
        t.begin(ID, 1).unwrap();
        assert!(matches!(
            t.begin(ID, 5),
            Err(ProtocolError::InvalidParams(_))
        ));
        assert!(matches!(
            t.begin("other", 1),
            Err(ProtocolError::LimitExceeded { limit: 1, .. })
        ));
        t.abandon(ID);
        assert!(t.is_empty());
        t.begin("other", 1).unwrap();
    }

    #[tokio::test]
    async fn cancel_before_and_during_the_open_is_remembered() {
        let t = ProcessTable::default();
        let cancel = |id: &str| ProcessCancelParams {
            process_id: id.into(),
            grace_ms: Some(0),
        };
        // Before the frame was read: remembered, and the open is CANCELLED.
        let r = t.cancel(&cancel(ID)).await;
        assert!(!r.was_running);
        assert!(matches!(t.begin(ID, 4), Err(ProtocolError::Cancelled(_))));
        // Remembered once only.
        t.begin(ID, 4).unwrap();

        // Pending (frame read, supervisor not started): the start is refused.
        let r = t.cancel(&cancel(ID)).await;
        assert!(!r.was_running);
        let (_rd, wr) = std::io::pipe().unwrap();
        let control = pipe::Sender::from_owned_fd(OwnedFd::from(wr)).unwrap();
        let res = t.start(ID, control, || panic!("must not spawn"));
        assert!(matches!(res, Err(ProtocolError::Cancelled(_))));
        assert!(t.is_empty());
    }

    #[test]
    fn spec_round_trips_through_a_frame() {
        let mut params = ProcessOpenParams::new(ID, vec!["/bin/true".into()]);
        params.env.insert("A".into(), "b".into());
        let frame = encode_frame(&Spec {
            request_id: RequestId::Number(7),
            params: params.clone(),
        })
        .unwrap();
        let (mut rd, mut wr) = std::io::pipe().unwrap();
        std::io::Write::write_all(&mut wr, &frame).unwrap();
        drop(wr);
        let mut header = [0u8; HEADER_LEN];
        rd.read_exact(&mut header).unwrap();
        let len = parse_header(header).unwrap();
        let mut payload = vec![0; len];
        rd.read_exact(&mut payload).unwrap();
        let spec: Spec = serde_json::from_slice(&payload).unwrap();
        assert_eq!(spec.params, params);
        assert_eq!(spec.request_id, RequestId::Number(7));
    }
}
