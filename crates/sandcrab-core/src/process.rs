//! Host side of `process.open`: one program per dedicated `crabd` connection, its stdin and
//! output as records ([`sandcrab_protocol::record`]), and `process.cancel` on the sandbox's
//! tool connection.
//!
//! [`open_process`] opens a fresh connection through the sandbox's [`Connector`] (which sends
//! the `auth` frame first, like every connection), writes the `process.open` frame, reads
//! exactly the answer and hands back a [`ProcessStream`]. [`ProcessTable`] adds the per-sandbox
//! rules on top: one open per `process_id` at a time, and a cancel can never lose a race with
//! the open (see [`ProcessTable::open`]).
//!
//! **Outcomes of an open.**
//!
//! | What happened | Result |
//! | --- | --- |
//! | Nothing could be written: connect, `auth` or the write of the request failed | [`SandboxError::Unavailable`]. Nothing ran; trying again may work |
//! | `crabd` answered an error (`SPAWN_FAILED`, `LIMIT_EXCEEDED`, `CANCELLED`, `-32602`) | [`SandboxError::Protocol`] with the typed error |
//! | The connection closed, or stayed silent past the timeout, after the request was written | [`SandboxError::Lost`]: the program may have run |
//! | The ack | A [`ProcessStream`]; the program runs |
//!
//! After the ack, [`ProcessOutput::next`] yields `STDOUT` / `STDERR` in the order the guest
//! sent them and the exit last. A connection that ends without `EXIT` (or sends a bad record)
//! is [`SandboxError::Lost`].
//!
//! **Bounded memory, backpressure.** Nothing reads the connection in the background. Each
//! [`ProcessOutput::next`] reads exactly one record, so the host holds at most one record per
//! direction: 64 KiB of data plus its 5-byte header, or one `EXIT` of at most 256 KiB. A
//! caller that stops calling `next` stops reading; the transport's buffers fill, the
//! supervisor's write blocks, its pipe fills, and the program blocks. stdin is the mirror
//! image: [`ProcessStdin::write`] returns once its records were written to the connection, so
//! a program that does not read its stdin blocks the writer.
//!
//! **Closing.** Dropping both halves of a [`ProcessStream`] closes the connection, which
//! `crabd` treats as ask, wait `kill_grace_ms`, force: nothing outlives the stream.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use sandcrab_protocol::{
    ProcessCancelParams, ProcessCancelResult, ProcessExit, ProcessOpenAck, ProcessOpenParams,
    ProtocolError, RECORD_HEADER_LEN, RecordChannel, RecordKind, StdinMode, encode_frame,
    encode_record_header, parse_record_header, record::MAX_RECORD_DATA_LEN,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};

use crate::client::{BoxedStream, Connector, CrabdClient, read_one_response};
use crate::{Result, SandboxError};

/// Default bound on connect, `auth`, the `process.open` frame and its answer.
pub const DEFAULT_PROCESS_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// One event from a running process, in the order the guest sent them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEvent {
    /// A `STDOUT` record (stdout, and stderr with `merge`). At most 64 KiB.
    Stdout(Vec<u8>),
    /// A `STDERR` record (only with `stderr: "stream"`). At most 64 KiB.
    Stderr(Vec<u8>),
    /// The `EXIT` record: always the last event.
    Exit(ProcessExit),
}

/// Open `params` on a fresh connection from `connector`. See the module docs for the
/// outcomes; `timeout` bounds everything up to the ack.
///
/// This is the bare protocol step. Callers that may also cancel the process use
/// [`ProcessTable::open`], which makes a cancel and an open that race safe.
pub async fn open_process(
    connector: &dyn Connector,
    params: &ProcessOpenParams,
    timeout: Duration,
) -> Result<ProcessStream> {
    params.validate()?;
    let deadline = tokio::time::Instant::now() + timeout;
    let stream = connect(connector, deadline).await?;
    finish_open(stream, params, deadline, None).await
}

async fn connect(connector: &dyn Connector, deadline: tokio::time::Instant) -> Result<BoxedStream> {
    match tokio::time::timeout_at(deadline, connector.connect()).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(SandboxError::Unavailable(format!(
            "cannot connect to crabd for process.open: {e}"
        ))),
        Err(_) => Err(SandboxError::Unavailable(
            "connecting to crabd for process.open timed out".into(),
        )),
    }
}

/// Write the request and read the answer on an open connection.
async fn finish_open(
    mut stream: BoxedStream,
    params: &ProcessOpenParams,
    deadline: tokio::time::Instant,
    registration: Option<Arc<Registration>>,
) -> Result<ProcessStream> {
    let frame = encode_frame(&params.to_request(1)).map_err(ProtocolError::from)?;
    // A write that fails leaves `crabd` without a complete frame: nothing was started.
    let write = async {
        stream.write_all(&frame).await?;
        stream.flush().await
    };
    match tokio::time::timeout_at(deadline, write).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return Err(SandboxError::Unavailable(format!(
                "cannot send process.open: {e}"
            )));
        }
        Err(_) => {
            return Err(SandboxError::Unavailable(
                "sending process.open timed out".into(),
            ));
        }
    }
    // From here on the program may have started.
    let resp = match tokio::time::timeout_at(deadline, read_one_response(&mut stream)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Err(SandboxError::Lost(format!(
                "no answer to process.open: {e}"
            )));
        }
        Err(_) => {
            return Err(SandboxError::Lost(
                "no answer to process.open before the timeout".into(),
            ));
        }
    };
    let ack = ProcessOpenAck::from_value(resp.into_result()?)
        .map_err(|e| SandboxError::Lost(e.to_string()))?;
    if ack.process_id != params.process_id {
        return Err(SandboxError::Lost(format!(
            "crabd acked process {} instead of {}",
            ack.process_id, params.process_id
        )));
    }
    let (r, w) = tokio::io::split(stream);
    Ok(ProcessStream {
        ack,
        stdin: ProcessStdin {
            writer: w,
            mode: params.stdin,
            closed: false,
            _registration: registration.clone(),
        },
        output: ProcessOutput {
            reader: r,
            header: [0; RECORD_HEADER_LEN],
            header_filled: 0,
            current: None,
            payload: Vec::new(),
            payload_filled: 0,
            state: OutputState::Running,
            _registration: registration,
        },
    })
}

/// A running process: its stdin and its output, over one connection.
pub struct ProcessStream {
    ack: ProcessOpenAck,
    stdin: ProcessStdin,
    output: ProcessOutput,
}

impl fmt::Debug for ProcessStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessStream")
            .field("process_id", &self.ack.process_id)
            .field("pid", &self.ack.pid)
            .finish_non_exhaustive()
    }
}

impl ProcessStream {
    #[must_use]
    pub fn process_id(&self) -> &str {
        &self.ack.process_id
    }

    /// The guest pid of the program. Never hand it to a client (design §2.4, invariant 9).
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.ack.pid
    }

    #[must_use]
    pub fn ack(&self) -> &ProcessOpenAck {
        &self.ack
    }

    pub fn stdin(&mut self) -> &mut ProcessStdin {
        &mut self.stdin
    }

    pub fn output(&mut self) -> &mut ProcessOutput {
        &mut self.output
    }

    /// Separate the halves, to drive stdin and output from different tasks. The connection
    /// closes when both are dropped.
    #[must_use]
    pub fn into_split(self) -> (ProcessStdin, ProcessOutput) {
        (self.stdin, self.output)
    }
}

/// The stdin half of a [`ProcessStream`].
pub struct ProcessStdin {
    writer: WriteHalf<BoxedStream>,
    mode: StdinMode,
    closed: bool,
    _registration: Option<Arc<Registration>>,
}

impl fmt::Debug for ProcessStdin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessStdin")
            .field("mode", &self.mode)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl ProcessStdin {
    /// Send `data` as `STDIN` records of at most 64 KiB. Returns once they were written to
    /// the connection (backpressure). Not cancel-safe: a dropped call may have sent part of
    /// a record, so drop the stream after one.
    ///
    /// Refused (nothing sent) when stdin is not a pipe or was closed: `crabd` closes the
    /// connection on such a record. A failed write is [`SandboxError::Lost`].
    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        if self.mode != StdinMode::Pipe {
            return Err(SandboxError::InvalidConfig(
                "the process was opened with `stdin: null`".into(),
            ));
        }
        if self.closed {
            return Err(SandboxError::InvalidConfig(
                "the process's stdin is already closed".into(),
            ));
        }
        for chunk in data.chunks(MAX_RECORD_DATA_LEN) {
            let header = encode_record_header(RecordKind::Stdin, chunk.len())
                .expect("chunk is within the record limit");
            self.send(&header, chunk).await?;
        }
        Ok(())
    }

    /// Send `STDIN_EOF`: the program's stdin reaches end-of-file, its output stays open.
    /// Idempotent; does nothing with `stdin: null`.
    pub async fn close(&mut self) -> Result<()> {
        if self.mode != StdinMode::Pipe || self.closed {
            return Ok(());
        }
        let header = encode_record_header(RecordKind::StdinEof, 0).expect("empty record is valid");
        self.send(&header, &[]).await?;
        self.closed = true;
        Ok(())
    }

    async fn send(&mut self, header: &[u8], payload: &[u8]) -> Result<()> {
        let res = async {
            self.writer.write_all(header).await?;
            if !payload.is_empty() {
                self.writer.write_all(payload).await?;
            }
            self.writer.flush().await
        }
        .await;
        res.map_err(|e| SandboxError::Lost(format!("process connection failed: {e}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputState {
    Running,
    Exited,
    Lost,
}

/// The output half of a [`ProcessStream`].
pub struct ProcessOutput {
    reader: ReadHalf<BoxedStream>,
    header: [u8; RECORD_HEADER_LEN],
    header_filled: usize,
    /// Kind and length of the record whose payload is being read.
    current: Option<(RecordKind, usize)>,
    payload: Vec<u8>,
    payload_filled: usize,
    state: OutputState,
    _registration: Option<Arc<Registration>>,
}

impl fmt::Debug for ProcessOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessOutput")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl ProcessOutput {
    /// The next event; `Ok(None)` after the exit was returned. Reads one record and nothing
    /// more, so a caller that does not call it holds the program back (see the module docs).
    ///
    /// Cancel-safe: dropping the future loses no bytes, so it can sit in a `select!`.
    ///
    /// A connection that ends without `EXIT`, or a record that is invalid on this connection,
    /// is [`SandboxError::Lost`], and so is every later call.
    pub async fn next(&mut self) -> Result<Option<ProcessEvent>> {
        match self.state {
            OutputState::Exited => return Ok(None),
            OutputState::Lost => {
                return Err(SandboxError::Lost(
                    "the process connection already failed".into(),
                ));
            }
            OutputState::Running => {}
        }
        match self.read_record().await {
            Ok(event) => {
                if matches!(event, ProcessEvent::Exit(_)) {
                    self.state = OutputState::Exited;
                }
                Ok(Some(event))
            }
            Err(e) => {
                self.state = OutputState::Lost;
                Err(SandboxError::Lost(e))
            }
        }
    }

    /// True once the exit was returned.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state != OutputState::Running
    }

    async fn read_record(&mut self) -> std::result::Result<ProcessEvent, String> {
        while self.current.is_none() {
            let n = self
                .reader
                .read(&mut self.header[self.header_filled..])
                .await
                .map_err(|e| format!("process connection failed: {e}"))?;
            if n == 0 {
                return Err(if self.header_filled == 0 {
                    "the process connection closed without an exit".to_owned()
                } else {
                    "the process connection closed mid-record".to_owned()
                });
            }
            self.header_filled += n;
            if self.header_filled == RECORD_HEADER_LEN {
                let (kind, len) = parse_record_header(RecordChannel::ProcessOutput, self.header)
                    .map_err(|e| format!("crabd sent an invalid record: {e}"))?;
                self.current = Some((kind, len));
                self.payload.clear();
                self.payload.resize(len, 0);
                self.payload_filled = 0;
            }
        }
        let (kind, len) = self.current.expect("set above");
        while self.payload_filled < len {
            let n = self
                .reader
                .read(&mut self.payload[self.payload_filled..])
                .await
                .map_err(|e| format!("process connection failed: {e}"))?;
            if n == 0 {
                return Err("the process connection closed mid-record".to_owned());
            }
            self.payload_filled += n;
        }
        self.current = None;
        self.header_filled = 0;
        let payload = std::mem::take(&mut self.payload);
        Ok(match kind {
            RecordKind::Stdout => ProcessEvent::Stdout(payload),
            RecordKind::Stderr => ProcessEvent::Stderr(payload),
            RecordKind::Exit => {
                ProcessEvent::Exit(ProcessExit::from_payload(&payload).map_err(|e| e.to_string())?)
            }
            other => {
                return Err(format!(
                    "crabd sent a {other} record on a process connection"
                ));
            }
        })
    }
}

/// Per-sandbox table of `process.open` calls, so a cancel and an open that race always end
/// the process. An entry lives from [`Self::open`] until the stream (both halves) is dropped.
#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    /// The `process.open` frame is being written or was written.
    written: bool,
    /// A cancel was asked for, with this grace.
    cancel: Option<Option<u64>>,
}

/// Removes a table entry when the last half of its stream (or the failed open) is dropped.
struct Registration {
    table: Weak<Mutex<HashMap<String, Entry>>>,
    id: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(t) = self.table.upgrade() {
            t.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.id);
        }
    }
}

impl ProcessTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `process_id` is being opened or still has a stream.
    #[must_use]
    pub fn contains(&self, process_id: &str) -> bool {
        self.lock().contains_key(process_id)
    }

    /// Open `params` like [`open_process`], with these rules:
    ///
    /// - The id is in the table before the first await. A `process_id` that is already in it
    ///   is [`SandboxError::AlreadyExists`].
    /// - A cancel that came before the request was written: the request is never written and
    ///   the open fails with [`ProtocolError::Cancelled`]; nothing ran.
    /// - A cancel that came after the request was written goes to `crabd`. If `crabd` has not
    ///   read the request yet, it remembers the cancel (60 s) and answers the open `CANCELLED`.
    pub async fn open(
        &self,
        connector: &dyn Connector,
        params: &ProcessOpenParams,
        timeout: Duration,
    ) -> Result<ProcessStream> {
        params.validate()?;
        let id = params.process_id.clone();
        let registration = {
            let mut t = self.lock();
            if t.contains_key(&id) {
                return Err(SandboxError::AlreadyExists {
                    kind: "process",
                    id,
                });
            }
            t.insert(id.clone(), Entry::default());
            Arc::new(Registration {
                table: Arc::downgrade(&self.inner),
                id: id.clone(),
            })
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let stream = connect(connector, deadline).await?;
        {
            let mut t = self.lock();
            let entry = t.get_mut(&id).expect("registered above");
            if entry.cancel.is_some() {
                return Err(ProtocolError::Cancelled(
                    "cancelled before the process was opened".into(),
                )
                .into());
            }
            entry.written = true;
        }
        finish_open(stream, params, deadline, Some(registration)).await
    }

    /// `process.cancel` through `client` (the sandbox's tool connection): ask, wait the grace
    /// (default 5000 ms, 0 = force at once), force. Idempotent.
    ///
    /// For an open in this table whose request was not written yet, nothing is sent: the open
    /// fails with `CANCELLED` and this answers `was_running: false` at once.
    pub async fn cancel(
        &self,
        client: &CrabdClient,
        process_id: &str,
        grace_ms: Option<u64>,
    ) -> Result<ProcessCancelResult> {
        {
            let mut t = self.lock();
            if let Some(entry) = t.get_mut(process_id) {
                entry.cancel = Some(grace_ms);
                if !entry.written {
                    return Ok(ProcessCancelResult {
                        was_running: false,
                        tree_clean: true,
                    });
                }
            }
        }
        client
            .process_cancel(ProcessCancelParams {
                process_id: process_id.to_owned(),
                grace_ms,
            })
            .await
    }
}

#[cfg(test)]
mod tests;
