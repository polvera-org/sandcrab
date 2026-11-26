//! Process handle: one program in a sandbox with an exact argv and environment, its stdin and
//! output as byte chunks, cancel, and an exit that never fails.
//!
//! [`Process::spawn`] (or [`crate::Sandbox::spawn`]) validates the spec, returns at once and
//! opens the `process.open` connection behind the handle. Everything that goes wrong after
//! that is a [`ProcessOutcome`], never an error:
//!
//! | What happened | [`Process::exit`] |
//! | --- | --- |
//! | The program ended by itself | [`ProcessOutcome::Exited`] |
//! | Its `timeout` passed | [`ProcessOutcome::TimedOut`] |
//! | Its output passed an `output_cap` with [`Overflow::Kill`] | [`ProcessOutcome::Overflow`] |
//! | [`Process::cancel`] stopped it, or came before it started | [`ProcessOutcome::Cancelled`] |
//! | The program or its cwd could not be started (`ENOENT`, `EACCES`) | [`ProcessOutcome::SpawnFailed`] |
//! | `crabd` or the host refused the request (a limit, a bad value) | [`ProcessOutcome::Rejected`] |
//! | The request could not be sent: nothing ran, trying again may work | [`ProcessOutcome::Unavailable`] |
//! | The connection ended after the request and before the exit: it may have run | [`ProcessOutcome::Lost`] |
//!
//! **Output and backpressure.** [`OutputStream::next`] yields stdout and stderr chunks (at
//! most 64 KiB each) in the order the guest sent them. The handle holds at most three unread
//! chunks; beyond that it stops reading the connection and the program blocks on its pipe.
//! So a caller that awaits [`Process::exit`] while it holds an [`OutputStream`] it does not
//! read waits for ever once the program wrote more than that. Read the stream, drop it (what
//! still arrives is discarded), or use [`crate::Sandbox::exec`]. [`Process::cancel`] discards
//! unread output too, so the exit is always reached after it.
//!
//! **Dropping** a [`Process`] closes its connection: `crabd` asks the program's whole tree to
//! stop, waits the spec's `kill_grace` and forces it. Nothing outlives the handle.
//!
//! ```no_run
//! use std::time::Duration;
//! use sandcrab_sdk::{Output, ProcessOutcome, ProcessSpec, Sandbox, StdinMode};
//!
//! # async fn demo(sandbox: &Sandbox) -> sandcrab_sdk::Result<()> {
//! let spec = ProcessSpec::new(["/usr/bin/sort"])
//!     .env("LC_ALL", "C") // the exact environment: nothing is inherited
//!     .stdin(StdinMode::Pipe)
//!     .timeout(Duration::from_secs(30));
//! let (process, mut output) = sandbox.spawn(spec)?;
//!
//! process.write("b\na\n").await?;
//! process.close_stdin().await?;
//! while let Some(chunk) = output.next().await {
//!     if let Output::Stdout(bytes) = chunk {
//!         print!("{}", String::from_utf8_lossy(&bytes));
//!     }
//! }
//! match process.exit().await {
//!     ProcessOutcome::Exited(exit) => println!("code {:?}", exit.code),
//!     other => println!("did not finish: {other:?} (tree clean: {})", other.tree_clean()),
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::time::Duration;

use sandcrab_core::{ProcessEvent, Result, SandboxError};
use sandcrab_protocol::{
    ExecLimits, OutputCap, Overflow, ProcessCancelResult, ProcessExit, ProcessOpenParams,
    ProtocolError, SpawnTarget, StderrMode, StdinMode,
};
use tokio::sync::{OnceCell, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Timeout [`crate::Sandbox::exec`] applies when the spec has none.
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);
/// Output cap in bytes [`crate::Sandbox::exec`] applies when the spec has none.
pub const DEFAULT_EXEC_OUTPUT_CAP: u64 = 1024 * 1024;

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What to run. Only `argv` is required; see [`ProcessOpenParams`] for the limits of each
/// field (`crabd` checks them again).
#[derive(Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    /// `argv[0]` is the program. No shell is involved.
    pub argv: Vec<String>,
    /// The exact environment. Nothing is added, nothing inherited.
    pub env: BTreeMap<String, String>,
    /// Absolute. Default: the sandbox's default cwd.
    pub cwd: Option<String>,
    /// Default: `/dev/null`.
    pub stdin: StdinMode,
    /// Default: not streamed, the tail is in the exit.
    pub stderr: StderrMode,
    /// Bytes of stderr kept for the exit. Default 4096, at most 65536.
    pub stderr_tail_bytes: Option<u32>,
    pub limits: Option<ExecLimits>,
    /// 0..=1000; only raises the score.
    pub oom_score_adj: Option<u16>,
    /// Wall-clock bound, timed in the guest. `None`: no bound.
    pub timeout: Option<Duration>,
    /// Grace between ask and force at the timeout, at the cap and on a dropped handle.
    /// Default 500 ms.
    pub kill_grace: Option<Duration>,
    pub output_cap: Option<OutputCap>,
}

/// Never prints environment values: they may be secrets.
impl fmt::Debug for ProcessSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessSpec")
            .field("argv", &self.argv)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .field("stdin", &self.stdin)
            .field("stderr", &self.stderr)
            .field("stderr_tail_bytes", &self.stderr_tail_bytes)
            .field("limits", &self.limits)
            .field("oom_score_adj", &self.oom_score_adj)
            .field("timeout", &self.timeout)
            .field("kill_grace", &self.kill_grace)
            .field("output_cap", &self.output_cap)
            .finish()
    }
}

impl ProcessSpec {
    #[must_use]
    pub fn new<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
            cwd: None,
            stdin: StdinMode::default(),
            stderr: StderrMode::default(),
            stderr_tail_bytes: None,
            limits: None,
            oom_score_adj: None,
            timeout: None,
            kill_grace: None,
            output_cap: None,
        }
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    #[must_use]
    pub fn stdin(mut self, mode: StdinMode) -> Self {
        self.stdin = mode;
        self
    }

    #[must_use]
    pub fn stderr(mut self, mode: StderrMode) -> Self {
        self.stderr = mode;
        self
    }

    #[must_use]
    pub fn stderr_tail_bytes(mut self, bytes: u32) -> Self {
        self.stderr_tail_bytes = Some(bytes);
        self
    }

    #[must_use]
    pub fn limits(mut self, limits: ExecLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    #[must_use]
    pub fn oom_score_adj(mut self, score: u16) -> Self {
        self.oom_score_adj = Some(score);
        self
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    pub fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = Some(grace);
        self
    }

    /// Bound stdout (and stderr with [`StderrMode::Merge`]) to `max_bytes`.
    #[must_use]
    pub fn output_cap(mut self, max_bytes: u64, on_overflow: Overflow) -> Self {
        self.output_cap = Some(OutputCap {
            max_bytes,
            on_overflow,
        });
        self
    }

    /// The `process.open` params for this spec under `process_id` (a UUID).
    #[must_use]
    pub fn to_params(&self, process_id: impl Into<String>) -> ProcessOpenParams {
        ProcessOpenParams {
            process_id: process_id.into(),
            argv: self.argv.clone(),
            env: self.env.clone(),
            cwd: self.cwd.clone(),
            stdin: self.stdin,
            stderr: self.stderr,
            stderr_tail_bytes: self.stderr_tail_bytes,
            limits: self.limits,
            oom_score_adj: self.oom_score_adj,
            timeout_ms: self.timeout.map(millis),
            kill_grace_ms: self.kill_grace.map(millis),
            output_cap: self.output_cap,
        }
    }
}

/// One chunk of a program's output, at most 64 KiB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// stdout, and stderr with [`StderrMode::Merge`].
    Stdout(Vec<u8>),
    /// Only with [`StderrMode::Stream`]. This stream has no cap.
    Stderr(Vec<u8>),
}

/// The output of a [`Process`], in the order the guest sent it. One consumer. See the
/// [module docs](self) for backpressure and for what dropping it does.
#[derive(Debug)]
pub struct OutputStream {
    rx: mpsc::Receiver<Output>,
}

impl OutputStream {
    /// The next chunk; `None` when the output has ended (the exit follows), when
    /// [`Process::cancel`] was called, or when the [`Process`] was dropped. Cancel-safe.
    pub async fn next(&mut self) -> Option<Output> {
        self.rx.recv().await
    }
}

/// How a process ended. See the [module docs](self) for when each variant is used.
#[derive(Debug, Clone)]
pub enum ProcessOutcome {
    /// The program ran and ended by itself (any exit code, or a signal nobody here sent).
    Exited(ProcessExit),
    /// The program was stopped at its timeout.
    TimedOut(ProcessExit),
    /// The program was stopped at its output cap.
    Overflow(ProcessExit),
    /// Cancelled. `None`: before the program started, nothing ran.
    Cancelled(Option<ProcessExit>),
    /// The program could not be started; nothing ran.
    SpawnFailed {
        /// The errno name, e.g. `ENOENT`, `EACCES`.
        errno: String,
        what: SpawnTarget,
    },
    /// `crabd` or the host refused the request; nothing ran. Trying again unchanged fails
    /// again, except for [`ProtocolError::LimitExceeded`] once other processes have ended.
    Rejected(SandboxError),
    /// The request was never sent (sandbox gone, `crabd` unreachable); nothing ran.
    Unavailable(String),
    /// The connection ended after the request was sent and without an exit. The program
    /// may have run and may have had effects: never retry as if nothing ran.
    Lost(String),
}

impl ProcessOutcome {
    /// Classify an `EXIT` record. A cancel wins over a timeout, a timeout over the cap.
    #[must_use]
    pub fn from_exit(exit: ProcessExit) -> Self {
        if exit.cancelled {
            Self::Cancelled(Some(exit))
        } else if exit.timed_out {
            Self::TimedOut(exit)
        } else if exit.overflow {
            Self::Overflow(exit)
        } else {
            Self::Exited(exit)
        }
    }

    /// Classify a failed `process.open`.
    #[must_use]
    pub fn from_open_error(error: SandboxError) -> Self {
        match error {
            SandboxError::Unavailable(m) => Self::Unavailable(m),
            SandboxError::Lost(m) => Self::Lost(m),
            SandboxError::Protocol(ProtocolError::SpawnFailed { errno, what }) => {
                Self::SpawnFailed { errno, what }
            }
            SandboxError::Protocol(ProtocolError::Cancelled(_)) => Self::Cancelled(None),
            other => Self::Rejected(other),
        }
    }

    /// The `EXIT` record, when the program ran to an end that `crabd` saw.
    #[must_use]
    pub fn exit_record(&self) -> Option<&ProcessExit> {
        match self {
            Self::Exited(e) | Self::TimedOut(e) | Self::Overflow(e) | Self::Cancelled(Some(e)) => {
                Some(e)
            }
            _ => None,
        }
    }

    /// The exit code, when the program ended with one.
    #[must_use]
    pub fn code(&self) -> Option<i32> {
        self.exit_record().and_then(|e| e.code)
    }

    /// True only for a program that ended by itself with code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        matches!(self, Self::Exited(e) if e.code == Some(0))
    }

    /// Whether nothing of the program's tree is left. True when nothing ran; false when
    /// `crabd` could not stop something and for [`Self::Lost`] (unknown). On false, destroy
    /// the sandbox.
    #[must_use]
    pub fn tree_clean(&self) -> bool {
        match self {
            Self::Lost(_) => false,
            other => other.exit_record().is_none_or(|e| e.tree_clean),
        }
    }

    /// The last bytes of stderr (empty with [`StderrMode::Merge`] and when nothing ran).
    #[must_use]
    pub fn stderr_tail(&self) -> &str {
        self.exit_record().map_or("", |e| e.stderr_tail.as_str())
    }
}

enum StdinCmd {
    Write(Vec<u8>, oneshot::Sender<Result<()>>),
    Close(oneshot::Sender<Result<()>>),
}

/// A program running (or about to run) in a sandbox. See the [module docs](self).
///
/// Every method takes `&self`, so the handle can be shared (`Arc`) between a task that
/// writes stdin, one that waits for the exit and one that may cancel.
pub struct Process {
    id: String,
    sandbox: sandcrab_core::Sandbox,
    stdin_mode: StdinMode,
    stdin: mpsc::Sender<StdinCmd>,
    started: watch::Receiver<Option<bool>>,
    outcome: watch::Receiver<Option<ProcessOutcome>>,
    discard: watch::Sender<bool>,
    cancelled: OnceCell<ProcessCancelResult>,
    driver: JoinHandle<()>,
}

impl fmt::Debug for Process {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Process")
            .field("id", &self.id)
            .field("sandbox", &self.sandbox.id())
            .field("finished", &self.outcome.borrow().is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Drops the connection (or the open in flight): crabd stops the tree.
        self.driver.abort();
    }
}

impl Process {
    /// Start `spec` in `sandbox`. Returns at once; the connection is opened in the
    /// background. Fails only for a malformed spec ([`ProtocolError::InvalidParams`]),
    /// before anything starts. Must be called inside a tokio runtime.
    pub fn spawn(
        sandbox: &sandcrab_core::Sandbox,
        spec: &ProcessSpec,
    ) -> Result<(Self, OutputStream)> {
        let id = Uuid::new_v4().to_string();
        let params = spec.to_params(id.clone());
        params.validate()?;
        // One chunk waits here, one in the driver, one in the connection reader.
        let (out_tx, out_rx) = mpsc::channel(1);
        let (stdin_tx, stdin_rx) = mpsc::channel(1);
        let (started_tx, started) = watch::channel(None);
        let (outcome_tx, outcome) = watch::channel(None);
        let (discard, discard_rx) = watch::channel(false);
        let driver = tokio::spawn(drive(
            sandbox.clone(),
            params,
            out_tx,
            stdin_rx,
            discard_rx,
            started_tx,
            outcome_tx,
        ));
        Ok((
            Self {
                id,
                sandbox: sandbox.clone(),
                stdin_mode: spec.stdin,
                stdin: stdin_tx,
                started,
                outcome,
                discard,
                cancelled: OnceCell::new(),
                driver,
            },
            OutputStream { rx: out_rx },
        ))
    }

    /// The process id (a UUID chosen here). The guest pid is never exposed.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// True once the program runs; false if it will never run (see [`Self::exit`] for why).
    pub async fn started(&self) -> bool {
        let mut rx = self.started.clone();
        rx.wait_for(Option::is_some)
            .await
            .is_ok_and(|v| *v == Some(true))
    }

    /// Write to the program's stdin. Returns once the bytes were written to the connection,
    /// so a program that does not read holds the writer back; bytes written before the
    /// program started are sent, in order, once it did. Dropping the returned future never
    /// leaves half a write behind.
    ///
    /// Writes to a process that has gone (or never started) are dropped without an error.
    /// The only errors are misuse: the spec did not ask for [`StdinMode::Pipe`], or stdin
    /// was already closed.
    pub async fn write(&self, data: impl Into<Vec<u8>>) -> Result<()> {
        if self.stdin_mode != StdinMode::Pipe {
            return Err(SandboxError::InvalidConfig(
                "the process was spawned without `stdin: pipe`".into(),
            ));
        }
        self.stdin_cmd(|ack| StdinCmd::Write(data.into(), ack))
            .await
    }

    /// End the program's stdin (its output stays open). Idempotent; does nothing without
    /// [`StdinMode::Pipe`] or once the process has gone.
    pub async fn close_stdin(&self) -> Result<()> {
        self.stdin_cmd(StdinCmd::Close).await
    }

    async fn stdin_cmd(
        &self,
        cmd: impl FnOnce(oneshot::Sender<Result<()>>) -> StdinCmd,
    ) -> Result<()> {
        let (ack, done) = oneshot::channel();
        if self.stdin.send(cmd(ack)).await.is_err() {
            return Ok(());
        }
        match done.await {
            Ok(Err(e @ SandboxError::InvalidConfig(_))) => Err(e),
            // Written, or the process has gone: the exit says how it ended.
            _ => Ok(()),
        }
    }

    /// Stop the program and everything it started: ask, wait `grace` (default 5 s, zero =
    /// force at once), force. Returns when nothing is left, with `tree_clean: false` if
    /// something could not be stopped or the sandbox could not be asked (destroy the sandbox
    /// then). Never fails. Idempotent: later calls return the first answer (their `grace` is
    /// not used). Unread output is discarded from here on, so [`Self::exit`] resolves.
    ///
    /// A cancel before the program started prevents the start: the exit is
    /// [`ProcessOutcome::Cancelled`]`(None)`.
    pub async fn cancel(&self, grace: Option<Duration>) -> ProcessCancelResult {
        self.discard.send_replace(true);
        if let Some(outcome) = self.outcome.borrow().as_ref() {
            return ProcessCancelResult {
                was_running: false,
                tree_clean: outcome.tree_clean(),
            };
        }
        let asked = self
            .cancelled
            .get_or_try_init(|| self.sandbox.cancel_process(&self.id, grace.map(millis)))
            .await;
        match asked {
            Ok(result) => result.clone(),
            Err(e) => {
                // A destroyed sandbox took the tree with it.
                let gone = self.sandbox.is_destroyed();
                if !gone {
                    tracing::warn!(process = %self.id, error = %e, "process.cancel failed");
                }
                ProcessCancelResult {
                    was_running: false,
                    tree_clean: gone,
                }
            }
        }
    }

    /// How the process ended. Resolves after the last output chunk was handed to the
    /// [`OutputStream`] (or discarded). Never fails; may be awaited any number of times.
    pub async fn exit(&self) -> ProcessOutcome {
        let mut rx = self.outcome.clone();
        match rx.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().expect("checked by wait_for"),
            // The runtime stopped the driver: the connection is closed, the end unknown.
            Err(_) => ProcessOutcome::Lost("the process driver stopped".into()),
        }
    }

    /// The outcome if the process has ended.
    #[must_use]
    pub fn try_exit(&self) -> Option<ProcessOutcome> {
        self.outcome.borrow().clone()
    }
}

/// Owns the connection: opens it, pumps stdin and output, publishes the outcome.
async fn drive(
    sandbox: sandcrab_core::Sandbox,
    params: ProcessOpenParams,
    out_tx: mpsc::Sender<Output>,
    stdin_rx: mpsc::Receiver<StdinCmd>,
    discard: watch::Receiver<bool>,
    started: watch::Sender<Option<bool>>,
    outcome: watch::Sender<Option<ProcessOutcome>>,
) {
    let stream = match sandbox.open_process(params).await {
        Ok(s) => s,
        Err(e) => {
            started.send_replace(Some(false));
            // Queued stdin writes are answered (dropped) before the exit is visible.
            drop(stdin_rx);
            outcome.send_replace(Some(ProcessOutcome::from_open_error(e)));
            return;
        }
    };
    started.send_replace(Some(true));
    let (stdin, output) = stream.into_split();
    // Two pumps on one task: a program that blocks on stdout while the caller blocks on its
    // stdin still makes progress as soon as either side moves.
    let end = tokio::select! {
        end = pump_output(output, out_tx, discard) => end,
        never = pump_stdin(stdin, stdin_rx) => match never {},
    };
    outcome.send_replace(Some(end));
}

async fn pump_stdin(
    mut stdin: sandcrab_core::ProcessStdin,
    mut rx: mpsc::Receiver<StdinCmd>,
) -> Infallible {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            StdinCmd::Write(data, ack) => {
                let _ = ack.send(stdin.write(&data).await);
            }
            StdinCmd::Close(ack) => {
                let _ = ack.send(stdin.close().await);
            }
        }
    }
    // The handle is gone; the driver is being aborted. Keep the stdin half until then.
    std::future::pending().await
}

async fn pump_output(
    mut output: sandcrab_core::ProcessOutput,
    tx: mpsc::Sender<Output>,
    mut discard: watch::Receiver<bool>,
) -> ProcessOutcome {
    let mut tx = Some(tx);
    loop {
        let chunk = match output.next().await {
            Ok(Some(ProcessEvent::Stdout(d))) => Output::Stdout(d),
            Ok(Some(ProcessEvent::Stderr(d))) => Output::Stderr(d),
            Ok(Some(ProcessEvent::Exit(exit))) => return ProcessOutcome::from_exit(exit),
            Ok(None) => return ProcessOutcome::Lost("the output ended without an exit".into()),
            Err(SandboxError::Lost(m)) => return ProcessOutcome::Lost(m),
            Err(e) => return ProcessOutcome::Lost(e.to_string()),
        };
        let Some(sender) = &tx else { continue };
        let delivered = tokio::select! {
            sent = sender.send(chunk) => sent.is_ok(),
            _ = discard.wait_for(|d| *d) => false,
        };
        if !delivered {
            // The stream was dropped or a cancel was asked: read on and discard.
            tx = None;
        }
    }
}

/// What [`crate::Sandbox::exec`] collected.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub outcome: ProcessOutcome,
    /// stdout (and stderr with [`StderrMode::Merge`]), bounded by the spec's `output_cap`.
    pub stdout: Vec<u8>,
    /// The streamed stderr with [`StderrMode::Stream`], at most the cap's `max_bytes` from
    /// its start. Empty otherwise: see [`ProcessOutcome::stderr_tail`].
    pub stderr: Vec<u8>,
    /// stderr was longer than the cap and was cut.
    pub stderr_truncated: bool,
}

/// Run `spec` to its end and collect its output. See [`crate::Sandbox::exec`].
pub(crate) async fn exec(
    sandbox: &sandcrab_core::Sandbox,
    mut spec: ProcessSpec,
) -> Result<ExecOutput> {
    spec.timeout.get_or_insert(DEFAULT_EXEC_TIMEOUT);
    let cap = spec
        .output_cap
        .get_or_insert(OutputCap {
            max_bytes: DEFAULT_EXEC_OUTPUT_CAP,
            on_overflow: Overflow::Kill,
        })
        .max_bytes;
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    // Dropped with this future: the program is stopped.
    let (process, mut output) = Process::spawn(sandbox, &spec)?;
    // No input: a program that reads stdin sees its end at once.
    process.close_stdin().await?;
    let mut out = ExecOutput {
        outcome: ProcessOutcome::Lost(String::new()),
        stdout: Vec::new(),
        stderr: Vec::new(),
        stderr_truncated: false,
    };
    while let Some(chunk) = output.next().await {
        match chunk {
            // The guest enforces the cap; the cut here only guards the host's memory.
            Output::Stdout(d) => {
                let room = cap.saturating_sub(out.stdout.len());
                out.stdout.extend_from_slice(&d[..d.len().min(room)]);
            }
            Output::Stderr(d) => {
                let room = cap.saturating_sub(out.stderr.len());
                out.stderr_truncated |= d.len() > room;
                out.stderr.extend_from_slice(&d[..d.len().min(room)]);
            }
        }
    }
    out.outcome = process.exit().await;
    Ok(out)
}

#[cfg(test)]
mod tests;
