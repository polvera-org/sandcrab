//! Background processes: `process_start` / `process_poll` / `process_stdin` /
//! `process_kill` / `process_list`, and the [`ProcessRegistry`] that tracks them.
//!
//! Contract (see the `Process*Params` / `Process*Result` types in `sandcrab-protocol`):
//! - `process_start`: spawn `bash -c <command>` in its own process group with cwd =
//!   `ctx.resolve_path_or_default(p.cwd)` and env + `p.env`; return immediately with a fresh
//!   `process_id` (opaque string, e.g. `p-<n>`) and the OS pid. `pty = true` attaches a
//!   pseudo-terminal (stdout carries the PTY output, stderr stays empty). More than
//!   `ctx.limits.process_max_count` tracked processes → `ExecutionFailed` (tell the caller
//!   to kill some).
//! - Output is drained continuously by background tasks into per-stream buffers bounded by
//!   `ctx.limits.process_max_buffer_bytes`; on overflow drop the oldest bytes and flag
//!   `truncated` for the next poll.
//! - `process_poll`: return and clear the output buffered since the previous poll, plus
//!   `running` / `exit_code` / `signal`. Unknown id → `NotFound`.
//! - `process_stdin`: write `data` to stdin (PTY master when `pty`); exited process or closed
//!   stdin → `ExecutionFailed`.
//! - `process_kill`: send `p.signal` to the whole process group (`killpg`);
//!   `was_running = false` if it had already exited. Not an error for exited processes.
//! - `process_list`: every tracked process, oldest first.
//! - [`ProcessRegistry::shutdown`] is called once by the server on graceful shutdown: kill
//!   every live process group (TERM, short grace, then KILL) and reap.
//! - Syscalls only via safe APIs (`unsafe_code = "forbid"`): tokio `process`, `rustix`.
//!
//! Implementation notes:
//! - PTY mode uses `pty-process`: the child becomes a session leader (`setsid`) with the PTY
//!   as its controlling terminal (so pgid == pid and `killpg` still covers the job), window
//!   size 24x80, and `TERM=xterm-256color` unless `p.env` sets `TERM`.
//! - Exit: once the leader (`bash`) exits, stragglers left in its group are SIGKILLed, the
//!   pipes are drained (bounded wait), and only then is the exit status published — so the
//!   poll that first reports `running = false` also carries the last output.
//! - Polled text never splits a UTF-8 sequence: an incomplete trailing sequence is held back
//!   until the next poll (or flushed lossily once the process has exited).
//! - Retention / GC: exited processes stay listed and pollable (their buffers keep the
//!   unpolled tail) until room is needed. When `process_start` finds
//!   `process_max_count` entries, it evicts exited entries — those whose exit was already
//!   reported by a poll first, then any exited, oldest first. If every entry is still
//!   running, the start fails with `ExecutionFailed`.
//! - `process_stdin` has no EOF / close operation in protocol v0.1; a write that blocks for
//!   [`STDIN_WRITE_TIMEOUT`] (process not reading) fails with `ExecutionFailed`.
//! - Known limitation (as for `bash`): descendants that leave the process group (`setsid`)
//!   escape `killpg`; the sandbox teardown is the backstop.
//! - Signal dispositions are inherited: if `crabd` itself was started with SIGINT ignored
//!   (e.g. `crabd &` from a non-interactive shell), `INT` has no effect on its children, and
//!   non-interactive bash always ignores SIGINT in `cmd &` jobs. Use `TERM` / `KILL` to stop
//!   a process reliably.

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rustix::process::{Pid, Signal};
use sandcrab_protocol::{
    KillSignal, ProcessInfo, ProcessKillParams, ProcessKillResult, ProcessListParams,
    ProcessListResult, ProcessPollParams, ProcessPollResult, ProcessStartParams,
    ProcessStartResult, ProcessStdinParams, ProcessStdinResult, ProtocolError,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::exec::{KILL_GRACE, READ_CHUNK, check_cwd, child_pgid, signal_group, spawn_error};
use crate::ToolContext;

/// How long output pumps may keep reading after the leader exited and its group was killed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Max time a single `process_stdin` write may block.
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Terminal size given to PTY processes.
const PTY_ROWS: u16 = 24;
const PTY_COLS: u16 = 80;

type StdinWriter = Box<dyn AsyncWrite + Send + Unpin>;
type OutputReader = Box<dyn AsyncRead + Send + Unpin>;
/// A spawned child plus its stdin writer and stdout / stderr readers.
type Spawned = (
    Child,
    Option<StdinWriter>,
    Option<OutputReader>,
    Option<OutputReader>,
);

/// Registry of background processes, held by [`ToolContext::processes`].
///
/// `Default + Send + Sync`; all state sits behind interior mutability.
#[derive(Default)]
pub struct ProcessRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_seq: u64,
    procs: BTreeMap<u64, Arc<Managed>>,
}

impl std::fmt::Debug for ProcessRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRegistry")
            .field("tracked", &lock(&self.inner).procs.len())
            .finish()
    }
}

/// One managed process.
struct Managed {
    id: String,
    pid: u32,
    pgid: Option<Pid>,
    command: String,
    pty: bool,
    state: Mutex<State>,
    stdin: tokio::sync::Mutex<Option<StdinWriter>>,
    /// Flips to `true` once the exit status is published.
    exited: watch::Receiver<bool>,
}

struct State {
    stdout: Ring,
    stderr: Ring,
    /// The leader was reaped (signals must no longer be sent: its pid may be reused).
    leader_exited: bool,
    /// Published exit status; set after output was drained.
    status: Option<ExitStatus>,
    /// A poll already reported the exit (eligible for eviction first).
    exit_reported: bool,
}

impl Managed {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn info(&self) -> ProcessInfo {
        let st = self.state();
        ProcessInfo {
            process_id: self.id.clone(),
            pid: self.pid,
            command: self.command.clone(),
            running: st.status.is_none(),
            exit_code: st.status.and_then(|s| s.code()),
            pty: self.pty,
        }
    }

    /// Signal the group unless the leader was already reaped. Returns whether it was sent.
    fn signal(&self, sig: Signal) -> bool {
        let st = self.state();
        if st.leader_exited {
            return false;
        }
        // Sent under the state lock so it cannot race with the waiter marking the exit.
        match self.pgid {
            Some(pgid) => rustix::process::kill_process_group(pgid, sig).is_ok(),
            None => false,
        }
    }

    async fn wait_exited(&self) {
        let mut rx = self.exited.clone();
        let _ = rx.wait_for(|v| *v).await;
    }
}

impl ProcessRegistry {
    /// Kill and reap every live process. Called by the server during graceful shutdown.
    ///
    /// SIGTERM to every live group, up to [`KILL_GRACE`] for them to exit, then SIGKILL and
    /// a bounded wait for the reaper tasks to publish the exits.
    pub async fn shutdown(&self) {
        let live: Vec<Arc<Managed>> = lock(&self.inner)
            .procs
            .values()
            .filter(|m| m.state().status.is_none())
            .cloned()
            .collect();
        if live.is_empty() {
            return;
        }
        for m in &live {
            m.signal(Signal::TERM);
        }
        let all = futures_join(&live);
        if tokio::time::timeout(KILL_GRACE, all).await.is_ok() {
            return;
        }
        for m in &live {
            m.signal(Signal::KILL);
        }
        let _ = tokio::time::timeout(DRAIN_TIMEOUT * 2, futures_join(&live)).await;
    }

    fn get(&self, id: &str) -> Result<Arc<Managed>, ProtocolError> {
        lock(&self.inner)
            .procs
            .values()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| {
                ProtocolError::NotFound(format!(
                    "no process with id `{id}`; call `process_list` to see tracked processes"
                ))
            })
    }
}

impl Drop for ProcessRegistry {
    fn drop(&mut self) {
        // Backstop if `shutdown` was never called: no process group outlives the registry.
        let inner = self.inner.get_mut().unwrap_or_else(PoisonError::into_inner);
        for m in inner.procs.values() {
            m.signal(Signal::KILL);
        }
    }
}

/// Wait until every process in `procs` has published its exit.
async fn futures_join(procs: &[Arc<Managed>]) {
    for m in procs {
        m.wait_exited().await;
    }
}

/// Start a background process.
pub async fn process_start(
    ctx: &ToolContext,
    p: ProcessStartParams,
) -> Result<ProcessStartResult, ProtocolError> {
    let cwd = ctx.resolve_path_or_default(p.cwd.as_deref());
    check_cwd(&cwd)?;
    let cap = ctx.limits.process_max_buffer_bytes;
    let reg = &ctx.processes;

    // The registry lock is held across the (synchronous) spawn so concurrent starts cannot
    // overshoot `process_max_count`.
    let mut inner = lock(&reg.inner);
    make_room(&mut inner, ctx.limits.process_max_count)?;

    let (child, stdin, out_src, err_src): Spawned = if p.pty {
        let (pty, pts) = pty_process::open().map_err(pty_error)?;
        pty.resize(pty_process::Size::new(PTY_ROWS, PTY_COLS))
            .map_err(pty_error)?;
        let mut cmd = pty_process::Command::new("bash")
            .arg("-c")
            .arg(&p.command)
            .current_dir(&cwd)
            .envs(&p.env)
            .kill_on_drop(true);
        if !p.env.contains_key("TERM") {
            cmd = cmd.env("TERM", "xterm-256color");
        }
        let child = cmd.spawn(pts).map_err(pty_error)?;
        let (r, w) = pty.into_split();
        (child, Some(Box::new(w)), Some(Box::new(r)), None)
    } else {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(&p.command)
            .current_dir(&cwd)
            .envs(&p.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| spawn_error(&e))?;
        let stdin = child.stdin.take().map(|s| Box::new(s) as StdinWriter);
        let out = child.stdout.take().map(|s| Box::new(s) as OutputReader);
        let err = child.stderr.take().map(|s| Box::new(s) as OutputReader);
        (child, stdin, out, err)
    };

    let pid = child.id().unwrap_or(0);
    let pgid = child_pgid(&child);
    inner.next_seq += 1;
    let seq = inner.next_seq;
    let (exited_tx, exited_rx) = watch::channel(false);
    let m = Arc::new(Managed {
        id: format!("p-{seq}"),
        pid,
        pgid,
        command: p.command,
        pty: p.pty,
        state: Mutex::new(State {
            stdout: Ring::new(cap),
            stderr: Ring::new(cap),
            leader_exited: false,
            status: None,
            exit_reported: false,
        }),
        stdin: tokio::sync::Mutex::new(stdin),
        exited: exited_rx,
    });
    inner.procs.insert(seq, Arc::clone(&m));
    drop(inner);

    let mut pumps = vec![];
    if let Some(src) = out_src {
        pumps.push(tokio::spawn(pump(src, Arc::clone(&m), Stream::Stdout)));
    }
    if let Some(src) = err_src {
        pumps.push(tokio::spawn(pump(src, Arc::clone(&m), Stream::Stderr)));
    }
    tokio::spawn(reap(child, Arc::clone(&m), pumps, exited_tx));

    Ok(ProcessStartResult {
        process_id: m.id.clone(),
        pid,
    })
}

/// Evict exited entries until there is room for one more process.
fn make_room(inner: &mut Inner, max: usize) -> Result<(), ProtocolError> {
    if inner.procs.len() < max {
        return Ok(());
    }
    let need = inner.procs.len() + 1 - max;
    let mut reported = vec![];
    let mut exited = vec![];
    for (seq, m) in &inner.procs {
        let st = m.state();
        if st.status.is_some() {
            if st.exit_reported {
                reported.push(*seq);
            } else {
                exited.push(*seq);
            }
        }
    }
    let victims: Vec<u64> = reported.into_iter().chain(exited).take(need).collect();
    if victims.len() < need {
        return Err(ProtocolError::ExecutionFailed(format!(
            "too many background processes ({} running, limit {max}); stop some with \
             `process_kill` before starting another",
            inner.procs.len()
        )));
    }
    for seq in victims {
        inner.procs.remove(&seq);
    }
    Ok(())
}

/// Wait for the leader, clean up its group, drain output, then publish the exit.
async fn reap(
    mut child: Child,
    m: Arc<Managed>,
    mut pumps: Vec<JoinHandle<()>>,
    exited: watch::Sender<bool>,
) {
    let status = child.wait().await;
    {
        let mut st = m.state();
        st.leader_exited = true;
        // Stragglers still in the group would keep the pipes open forever.
        signal_group(m.pgid, Signal::KILL);
    }
    let drain = async {
        for p in &mut pumps {
            let _ = p.await;
        }
    };
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
    for p in &pumps {
        p.abort();
    }
    *m.stdin.lock().await = None;
    // A failed wait (should not happen) is reported as killed by SIGKILL.
    let status = status.unwrap_or_else(|_| ExitStatus::from_raw(super::exec::SIGKILL));
    m.state().status = Some(status);
    let _ = exited.send(true);
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

async fn pump(mut src: impl AsyncRead + Unpin, m: Arc<Managed>, which: Stream) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match src.read(&mut buf).await {
            // A PTY master reports EIO once the slave side is closed.
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut st = m.state();
                match which {
                    Stream::Stdout => st.stdout.push(&buf[..n]),
                    Stream::Stderr => st.stderr.push(&buf[..n]),
                }
            }
        }
    }
}

/// Status and new output of a background process.
pub async fn process_poll(
    ctx: &ToolContext,
    p: ProcessPollParams,
) -> Result<ProcessPollResult, ProtocolError> {
    let m = ctx.processes.get(&p.process_id)?;
    let mut st = m.state();
    let done = st.status.is_some();
    let (stdout, t1) = st.stdout.take(done);
    let (stderr, t2) = st.stderr.take(done);
    if done {
        st.exit_reported = true;
    }
    Ok(ProcessPollResult {
        running: !done,
        exit_code: st.status.and_then(|s| s.code()),
        signal: st.status.and_then(|s| s.signal()),
        stdout,
        stderr,
        truncated: t1 || t2,
    })
}

/// Write to a background process's stdin.
pub async fn process_stdin(
    ctx: &ToolContext,
    p: ProcessStdinParams,
) -> Result<ProcessStdinResult, ProtocolError> {
    let m = ctx.processes.get(&p.process_id)?;
    if m.state().leader_exited {
        return Err(exited_error(&m.id));
    }
    let mut guard = m.stdin.lock().await;
    let Some(w) = guard.as_mut() else {
        return Err(exited_error(&m.id));
    };
    let data = p.data.as_bytes();
    let write = async {
        w.write_all(data).await?;
        w.flush().await
    };
    match tokio::time::timeout(STDIN_WRITE_TIMEOUT, write).await {
        Ok(Ok(())) => Ok(ProcessStdinResult {
            bytes_written: data.len() as u64,
        }),
        Ok(Err(e)) => {
            *guard = None;
            Err(ProtocolError::ExecutionFailed(format!(
                "stdin of process `{}` is closed ({e}); the process stopped reading input",
                m.id
            )))
        }
        Err(_) => Err(ProtocolError::ExecutionFailed(format!(
            "writing to stdin of process `{}` blocked for {} s: the process is not reading \
             its input; poll its output or kill it",
            m.id,
            STDIN_WRITE_TIMEOUT.as_secs()
        ))),
    }
}

fn exited_error(id: &str) -> ProtocolError {
    ProtocolError::ExecutionFailed(format!(
        "process `{id}` has exited, its stdin is closed; poll it for the exit status and start \
         a new process"
    ))
}

/// Signal a background process's group.
pub async fn process_kill(
    ctx: &ToolContext,
    p: ProcessKillParams,
) -> Result<ProcessKillResult, ProtocolError> {
    let m = ctx.processes.get(&p.process_id)?;
    let sig = match p.signal {
        KillSignal::Term => Signal::TERM,
        KillSignal::Int => Signal::INT,
        KillSignal::Kill => Signal::KILL,
    };
    Ok(ProcessKillResult {
        was_running: m.signal(sig),
    })
}

/// List tracked background processes.
pub async fn process_list(
    ctx: &ToolContext,
    p: ProcessListParams,
) -> Result<ProcessListResult, ProtocolError> {
    let ProcessListParams {} = p;
    let procs: Vec<Arc<Managed>> = lock(&ctx.processes.inner).procs.values().cloned().collect();
    Ok(ProcessListResult {
        processes: procs.iter().map(|m| m.info()).collect(),
    })
}

fn pty_error(e: pty_process::Error) -> ProtocolError {
    ProtocolError::ExecutionFailed(format!("failed to start process on a pseudo-terminal: {e}"))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bounded FIFO of unpolled bytes: overflow drops the oldest bytes and sets `truncated`.
struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    truncated: bool,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
            truncated: false,
        }
    }

    fn push(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            self.truncated |= !self.buf.is_empty() || data.len() > self.cap;
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.truncated = true;
            self.buf.drain(..overflow);
        }
        self.buf.extend(data);
    }

    /// Take buffered text and the truncation flag. Unless `flush`, an incomplete UTF-8
    /// sequence at the end stays buffered for the next call.
    fn take(&mut self, flush: bool) -> (String, bool) {
        let bytes: Vec<u8> = self.buf.drain(..).collect();
        let keep = if flush {
            0
        } else {
            incomplete_utf8_suffix(&bytes)
        };
        let (text, rest) = bytes.split_at(bytes.len() - keep);
        self.buf.extend(rest);
        let truncated = std::mem::take(&mut self.truncated);
        (String::from_utf8_lossy(text).into_owned(), truncated)
    }
}

/// Length of a trailing, possibly-completable UTF-8 sequence prefix (0..=3).
fn incomplete_utf8_suffix(b: &[u8]) -> usize {
    match std::str::from_utf8(b) {
        Ok(_) => 0,
        Err(e) => {
            // `error_len() == None` means the input ended mid-sequence.
            let tail = b.len() - e.valid_up_to();
            if e.error_len().is_none() {
                tail
            } else {
                // Invalid bytes somewhere; only hold back a short incomplete tail, if any.
                (1..=3.min(b.len()))
                    .rev()
                    .find(|&n| {
                        matches!(std::str::from_utf8(&b[b.len() - n..]),
                            Err(e) if e.valid_up_to() == 0 && e.error_len().is_none())
                    })
                    .unwrap_or(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Ring, incomplete_utf8_suffix};

    #[test]
    fn ring_drops_oldest() {
        let mut r = Ring::new(4);
        r.push(b"ab");
        r.push(b"cdef");
        assert_eq!(r.take(true), ("cdef".to_owned(), true));
        assert_eq!(r.take(true), (String::new(), false));
        r.push(b"abcdefgh");
        assert_eq!(r.take(true), ("efgh".to_owned(), true));
        r.push(b"abcd");
        assert_eq!(r.take(true), ("abcd".to_owned(), false));
    }

    #[test]
    fn ring_holds_back_split_utf8() {
        let mut r = Ring::new(64);
        let e = "é".as_bytes();
        r.push(b"x");
        r.push(&e[..1]);
        assert_eq!(r.take(false), ("x".to_owned(), false));
        r.push(&e[1..]);
        assert_eq!(r.take(false), ("é".to_owned(), false));
        r.push(&e[..1]);
        assert_eq!(r.take(true), ("\u{fffd}".to_owned(), false));
    }

    #[test]
    fn utf8_suffix() {
        assert_eq!(incomplete_utf8_suffix(b"abc"), 0);
        assert_eq!(incomplete_utf8_suffix(&"€".as_bytes()[..2]), 2);
        assert_eq!(incomplete_utf8_suffix(b"\xffab"), 0);
        assert_eq!(incomplete_utf8_suffix(b"\xffa\xe2\x82"), 2);
    }
}
