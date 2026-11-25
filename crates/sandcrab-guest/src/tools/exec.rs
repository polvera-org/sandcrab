//! `bash`: run a shell command to completion.
//!
//! Contract (see `BashParams` / `BashResult` in `sandcrab-protocol` and `docs/ROADMAP.md`):
//! - Run `bash -c <command>` with cwd = `ctx.resolve_path_or_default(p.cwd)`; the daemon env
//!   plus `p.env`. Missing cwd → `ProtocolError::NotFound`. Spawn failure →
//!   `ProtocolError::ExecutionFailed`. stdin is `/dev/null`.
//! - The child leads its **own process group** (`process_group(0)`); on timeout signal the
//!   whole group (`killpg`) so grandchildren die too, then reap. Timeout =
//!   `p.timeout_ms` or `ctx.limits.bash_default_timeout_ms`, capped at
//!   `ctx.limits.bash_max_timeout_ms`. A timeout is *not* an RPC error: return the result
//!   with `timed_out = true`, `exit_code = None` and the output so far.
//! - Capture stdout and stderr concurrently. Keep at most `ctx.limits.bash_max_output_bytes`
//!   per stream as head + tail with a `[... N bytes truncated ...]` marker in the middle
//!   (bounded memory while reading, never buffer unbounded output); set `truncated`.
//!   Decode lossily as UTF-8.
//! - `exit_code` / `signal` from the exit status; `duration_ms` = wall clock.
//! - If the request future is dropped (connection closed / daemon shutdown), the process
//!   group must still be killed (e.g. `kill_on_drop` + a group-kill guard).
//! - Syscalls only via safe APIs (`unsafe_code = "forbid"`): std / tokio `process_group`,
//!   `rustix` for `killpg`.
//!
//! Implementation notes:
//! - Timeout teardown is SIGTERM to the group, a [`KILL_GRACE`] grace period, then SIGKILL.
//!   `signal` reports the signal that actually terminated `bash` (15 or 9); if `bash` trapped
//!   TERM and exited by itself, `signal` is still `Some(15)` (the signal we sent).
//! - When `bash` exits normally, anything it left running in its process group (e.g.
//!   `server &`) is SIGKILLed: `bash` is run-to-completion, use `process_start` for
//!   long-lived work. Remaining pipe output is then drained for at most [`DRAIN_TIMEOUT`].
//! - Known limitation: a descendant that leaves the process group (`setsid`, `setpgid`)
//!   escapes the group kill. Its pipe ends are abandoned after [`DRAIN_TIMEOUT`]; the
//!   container / microVM teardown is the backstop for such processes.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use sandcrab_protocol::{BashParams, BashResult, ProtocolError};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::ToolContext;

/// Grace between SIGTERM and SIGKILL when tearing down a process group.
pub(crate) const KILL_GRACE: Duration = Duration::from_millis(500);
/// How long to keep reading pipes after `bash` exited and its group was killed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Read chunk size for output pumps.
pub(crate) const READ_CHUNK: usize = 16 * 1024;

pub(crate) const SIGTERM: i32 = 15;
pub(crate) const SIGKILL: i32 = 9;

/// Run `p.command` with `bash -c`. See the module docs for the contract.
pub async fn bash(ctx: &ToolContext, p: BashParams) -> Result<BashResult, ProtocolError> {
    let cwd = ctx.resolve_path_or_default(p.cwd.as_deref());
    check_cwd(&cwd)?;
    let timeout_ms = p
        .timeout_ms
        .unwrap_or(ctx.limits.bash_default_timeout_ms)
        .min(ctx.limits.bash_max_timeout_ms);
    let max = ctx.limits.bash_max_output_bytes;

    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(&p.command)
        .current_dir(&cwd)
        .envs(&p.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| spawn_error(&e))?;
    // Armed before anything can fail or be cancelled: SIGKILLs the group on drop.
    let mut guard = GroupGuard::new(&child);

    let out = Arc::new(Mutex::new(HeadTail::new(max)));
    let err = Arc::new(Mutex::new(HeadTail::new(max)));
    let mut pumps = vec![];
    if let Some(s) = child.stdout.take() {
        pumps.push(AbortOnDrop(tokio::spawn(pump(s, Arc::clone(&out)))));
    }
    if let Some(s) = child.stderr.take() {
        pumps.push(AbortOnDrop(tokio::spawn(pump(s, Arc::clone(&err)))));
    }

    let (status, timed_out) =
        match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
            Ok(status) => (status.map_err(wait_error)?, false),
            Err(_) => (terminate_group(&mut child, guard.pgid).await?, true),
        };
    // Reap stragglers left in the group (backgrounded jobs) so the pipes reach EOF.
    guard.kill(Signal::KILL);
    guard.disarm();

    let drain = async {
        for p in &mut pumps {
            let _ = (&mut p.0).await;
        }
    };
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
    drop(pumps);

    let (stdout, t1) = lock(&out).render();
    let (stderr, t2) = lock(&err).render();
    let (exit_code, signal) = if timed_out {
        (None, Some(status.signal().unwrap_or(SIGTERM)))
    } else {
        (status.code(), status.signal())
    };
    Ok(BashResult {
        exit_code,
        signal,
        stdout,
        stderr,
        truncated: t1 || t2,
        timed_out,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// SIGTERM the group, wait [`KILL_GRACE`], SIGKILL it, and reap the leader.
async fn terminate_group(
    child: &mut Child,
    pgid: Option<Pid>,
) -> Result<ExitStatus, ProtocolError> {
    signal_group(pgid, Signal::TERM);
    if let Ok(status) = tokio::time::timeout(KILL_GRACE, child.wait()).await {
        // Leader gone; make sure nothing in the group outlives it.
        signal_group(pgid, Signal::KILL);
        return status.map_err(wait_error);
    }
    signal_group(pgid, Signal::KILL);
    child.wait().await.map_err(wait_error)
}

/// `killpg`, ignoring errors (`ESRCH` once the group is gone).
pub(crate) fn signal_group(pgid: Option<Pid>, sig: Signal) {
    if let Some(pgid) = pgid {
        let _ = kill_process_group(pgid, sig);
    }
}

/// Process-group id of a child spawned as a group / session leader (pgid == pid).
pub(crate) fn child_pgid(child: &Child) -> Option<Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
}

/// `NotFound` / `ExecutionFailed` for a working directory that cannot be used.
pub(crate) fn check_cwd(cwd: &Path) -> Result<(), ProtocolError> {
    match std::fs::metadata(cwd) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(ProtocolError::ExecutionFailed(format!(
            "working directory {} is not a directory; pass a directory as `cwd`",
            cwd.display()
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ProtocolError::NotFound(format!(
            "working directory {} does not exist; create it first or pass an existing `cwd`",
            cwd.display()
        ))),
        Err(e) => Err(ProtocolError::ExecutionFailed(format!(
            "cannot use working directory {}: {e}",
            cwd.display()
        ))),
    }
}

pub(crate) fn spawn_error(e: &io::Error) -> ProtocolError {
    ProtocolError::ExecutionFailed(format!("failed to start `bash -c`: {e}"))
}

fn wait_error(e: io::Error) -> ProtocolError {
    ProtocolError::ExecutionFailed(format!("failed to wait for command: {e}"))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read `src` to EOF into `sink`. Read errors end the stream (a PTY master reports `EIO`
/// once the slave side is closed).
async fn pump(mut src: impl AsyncRead + Unpin, sink: Arc<Mutex<HeadTail>>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match src.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => lock(&sink).push(&buf[..n]),
        }
    }
}

/// SIGKILLs a process group when dropped, unless disarmed.
struct GroupGuard {
    pgid: Option<Pid>,
    armed: bool,
}

impl GroupGuard {
    fn new(child: &Child) -> Self {
        Self {
            pgid: child_pgid(child),
            armed: true,
        }
    }

    fn kill(&self, sig: Signal) {
        signal_group(self.pgid, sig);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill(Signal::KILL);
        }
    }
}

/// Aborts a spawned task when dropped.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Bounded capture: the first `head_cap` bytes plus a rolling window of the last
/// `tail_cap` bytes. Memory never exceeds `max` regardless of stream length.
#[derive(Debug)]
pub(crate) struct HeadTail {
    head: Vec<u8>,
    head_cap: usize,
    tail: std::collections::VecDeque<u8>,
    tail_cap: usize,
    total: u64,
}

impl HeadTail {
    pub(crate) fn new(max: usize) -> Self {
        let head_cap = max / 2;
        Self {
            head: Vec::new(),
            head_cap,
            tail: std::collections::VecDeque::new(),
            tail_cap: max - head_cap,
            total: 0,
        }
    }

    pub(crate) fn push(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        let room = self.head_cap - self.head.len();
        if room > 0 {
            let n = room.min(data.len());
            self.head.extend_from_slice(&data[..n]);
            data = &data[n..];
        }
        if self.tail_cap == 0 || data.is_empty() {
            return;
        }
        if data.len() >= self.tail_cap {
            self.tail.clear();
            self.tail.extend(&data[data.len() - self.tail_cap..]);
        } else {
            let overflow = (self.tail.len() + data.len()).saturating_sub(self.tail_cap);
            self.tail.drain(..overflow);
            self.tail.extend(data);
        }
    }

    /// Lossy UTF-8 text and whether anything was dropped.
    pub(crate) fn render(&self) -> (String, bool) {
        let kept = (self.head.len() + self.tail.len()) as u64;
        let dropped = self.total - kept;
        let mut s = String::from_utf8_lossy(&self.head).into_owned();
        if dropped > 0 {
            s.push_str(&format!("\n[... {dropped} bytes truncated ...]\n"));
        }
        let (a, b) = self.tail.as_slices();
        if b.is_empty() {
            s.push_str(&String::from_utf8_lossy(a));
        } else {
            s.push_str(&String::from_utf8_lossy(&[a, b].concat()));
        }
        (s, dropped > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::HeadTail;

    #[test]
    fn head_tail_small_is_verbatim() {
        let mut h = HeadTail::new(10);
        h.push(b"abc");
        h.push(b"def");
        assert_eq!(h.render(), ("abcdef".to_owned(), false));
    }

    #[test]
    fn head_tail_exact_fit_not_truncated() {
        let mut h = HeadTail::new(10);
        h.push(b"0123456789");
        assert_eq!(h.render(), ("0123456789".to_owned(), false));
    }

    #[test]
    fn head_tail_keeps_ends() {
        let mut h = HeadTail::new(10);
        for c in b'a'..=b'z' {
            h.push(&[c]);
        }
        assert_eq!(
            h.render(),
            (
                "abcde\n[... 16 bytes truncated ...]\nvwxyz".to_owned(),
                true
            )
        );
        let mut big = HeadTail::new(10);
        big.push(&(b'a'..=b'z').collect::<Vec<_>>());
        assert_eq!(big.render(), h.render());
    }

    #[test]
    fn head_tail_zero_cap() {
        let mut h = HeadTail::new(0);
        h.push(b"xyz");
        assert_eq!(
            h.render(),
            ("\n[... 3 bytes truncated ...]\n".to_owned(), true)
        );
    }
}
