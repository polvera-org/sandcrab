//! `process.open` and `process.cancel`: one program per dedicated connection, with an exact
//! argv and environment, stdin and output as [records](crate::record), and an in-band exit.
//!
//! A fresh connection (after `auth` where a token is set) sends one frame:
//!
//! ```text
//! -> {"jsonrpc":"2.0","id":1,"method":"process.open","params":{"process_id":"<uuid>","argv":["make"],"env":{}}}
//! <- {"jsonrpc":"2.0","id":1,"result":{"process_id":"<uuid>","pid":42}}
//! ... then records both ways (STDIN / STDIN_EOF in, STDOUT / STDERR / EXIT out) ...
//! ```
//!
//! The ack is sent only after the program was started, so an ack means it runs. A program
//! that could not be started gets an error frame instead ([`ProtocolError::SpawnFailed`],
//! [`ProtocolError::LimitExceeded`], [`ProtocolError::Cancelled`] or `-32602`) and `crabd`
//! closes the connection. `EXIT` ([`ProcessExit`]) is always the last record.
//!
//! `process.cancel` is an ordinary method on a tool connection, so a program that blocks its
//! stdin cannot hold a cancel up. Closing the process connection stops the program too (ask,
//! wait `kill_grace_ms`, force).
//!
//! Like `tunnel.open`, `process.open` is a connection-level verb: it is not in
//! [`crate::TOOL_NAMES`], not in `hello.tools`, and not part of [`crate::Call`]. It does not
//! share process ids with the `process_*` tools.

use std::{collections::BTreeMap, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    ProtocolError, RequestId, RpcRequest,
    record::{MAX_EXIT_RECORD_LEN, RecordKind, encode_record},
    tools::{from_params, to_json},
};

/// Connection-level verb that runs one program on a fresh connection.
pub const METHOD_PROCESS_OPEN: &str = "process.open";
/// Method on a tool connection that stops a `process.open` program and its tree.
pub const METHOD_PROCESS_CANCEL: &str = "process.cancel";

/// Default bytes of stderr kept for [`ProcessExit::stderr_tail`].
pub const DEFAULT_STDERR_TAIL_BYTES: u32 = 4096;
/// Largest accepted [`ProcessOpenParams::stderr_tail_bytes`].
pub const MAX_STDERR_TAIL_BYTES: u32 = 64 * 1024;
/// Default grace between ask (`SIGTERM`) and force (`SIGKILL`) at the timeout, at the output
/// cap and on a closed connection.
pub const DEFAULT_KILL_GRACE_MS: u64 = 500;
/// Default grace of `process.cancel`.
pub const DEFAULT_CANCEL_GRACE_MS: u64 = 5000;
/// Highest accepted `oom_score_adj` (the kernel's maximum).
pub const MAX_OOM_SCORE_ADJ: u16 = 1000;
/// Lowest accepted [`ExecLimits::data_bytes`] (32 MiB): below it most runtimes cannot start.
pub const MIN_DATA_BYTES: u64 = 32 * 1024 * 1024;
/// Lowest accepted [`ExecLimits::nofile`].
pub const MIN_NOFILE: u64 = 64;
/// Largest [`OutputCap::max_bytes`] with [`Overflow::Tail`]: the guest keeps the tail in memory.
pub const MAX_TAIL_CAP_BYTES: u64 = 8 * 1024 * 1024;
/// Default number of `process.open` programs open at once per sandbox (`--max-processes`).
pub const DEFAULT_MAX_PROCESSES: u32 = 64;
/// After the leader exited, the pipes are drained for at most this long before `EXIT`.
pub const DRAIN_AFTER_EXIT_MS: u64 = 2000;
/// The force loop of a tree kill gives up after this long (`tree_clean: false`).
pub const FORCE_LOOP_MS: u64 = 5000;

/// What the program's stdin is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StdinMode {
    /// `/dev/null`.
    #[default]
    Null,
    /// A pipe fed by `STDIN` records and closed by `STDIN_EOF`.
    Pipe,
}

/// What happens to the program's stderr.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StderrMode {
    /// Not streamed; the last `stderr_tail_bytes` are in [`ProcessExit::stderr_tail`].
    #[default]
    Tail,
    /// Sent with stdout, on one pipe, as `STDOUT` records. The tail is empty.
    Merge,
    /// Streamed as `STDERR` records; the tail is kept too.
    Stream,
}

/// What happens when stdout passes [`OutputCap::max_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Overflow {
    /// Stream as usual; cut the record at the cap and stop the tree.
    Kill,
    /// Do not stream; keep the last `max_bytes` and send them when the program has ended.
    Tail,
}

/// A bound on stdout (and stderr with [`StderrMode::Merge`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputCap {
    /// At least 1. At most 8 MiB with `tail`.
    pub max_bytes: u64,
    pub on_overflow: Overflow,
}

/// Resource limits applied to the program before it starts. Each `None` inherits the
/// sandbox's limit. A value above the sandbox's own hard limit is `-32602`, never clamped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecLimits {
    /// `RLIMIT_NPROC`. On Docker it counts every process of the uid on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nproc: Option<u64>,
    /// `RLIMIT_DATA`, at least 32 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_bytes: Option<u64>,
    /// `RLIMIT_FSIZE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fsize_bytes: Option<u64>,
    /// `RLIMIT_NOFILE`, at least 64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nofile: Option<u64>,
}

/// First frame of a process connection. Unknown fields are rejected, as for every params type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessOpenParams {
    /// Chosen by the caller: a UUID. Lets a cancel arrive before the ack.
    pub process_id: String,
    /// `argv[0]` is the program. Not empty. No element contains NUL.
    pub argv: Vec<String>,
    /// The exact environment. Nothing is added, nothing inherited.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Absolute. Default: the sandbox's default cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub stdin: StdinMode,
    #[serde(default)]
    pub stderr: StderrMode,
    /// Bytes of stderr kept for the exit with `tail` and `stream`. Default 4096, max 65536.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ExecLimits>,
    /// 0..=1000. Only raises the score. Default: unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_score_adj: Option<u16>,
    /// Wall-clock bound, timed in the guest. None: no bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Grace between ask and force at the bound, at the cap and on a closed connection.
    /// Default 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill_grace_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_cap: Option<OutputCap>,
}

impl ProcessOpenParams {
    /// Params with defaults for everything but the id, argv and env.
    #[must_use]
    pub fn new(process_id: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            process_id: process_id.into(),
            argv,
            env: BTreeMap::new(),
            cwd: None,
            stdin: StdinMode::default(),
            stderr: StderrMode::default(),
            stderr_tail_bytes: None,
            limits: None,
            oom_score_adj: None,
            timeout_ms: None,
            kill_grace_ms: None,
            output_cap: None,
        }
    }

    /// Checks that need no knowledge of the sandbox. Limits above the sandbox's hard limits
    /// are checked by `crabd`.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        let bad = |m: String| Err(ProtocolError::InvalidParams(m));
        if Uuid::parse_str(&self.process_id).is_err() {
            return bad("`process_id` must be a UUID".into());
        }
        if self.argv.is_empty() || self.argv[0].is_empty() {
            return bad("`argv` must name a program in `argv[0]`".into());
        }
        if self.argv.iter().any(|a| a.contains('\0')) {
            return bad("`argv` elements must not contain NUL".into());
        }
        for (k, v) in &self.env {
            // Never echo values: they may be secrets.
            if k.is_empty() || k.contains('=') || k.contains('\0') {
                return bad(format!("`env` has an invalid name {k:?}"));
            }
            if v.contains('\0') {
                return bad(format!("`env.{k}` must not contain NUL"));
            }
        }
        if let Some(cwd) = &self.cwd
            && (!cwd.starts_with('/') || cwd.contains('\0'))
        {
            return bad("`cwd` must be an absolute path".into());
        }
        if self
            .stderr_tail_bytes
            .is_some_and(|n| n > MAX_STDERR_TAIL_BYTES)
        {
            return bad(format!(
                "`stderr_tail_bytes` must be at most {MAX_STDERR_TAIL_BYTES}"
            ));
        }
        if let Some(l) = &self.limits {
            if l.data_bytes.is_some_and(|n| n < MIN_DATA_BYTES) {
                return bad(format!(
                    "`limits.data_bytes` must be at least {MIN_DATA_BYTES}"
                ));
            }
            if l.nofile.is_some_and(|n| n < MIN_NOFILE) {
                return bad(format!("`limits.nofile` must be at least {MIN_NOFILE}"));
            }
        }
        if self.oom_score_adj.is_some_and(|n| n > MAX_OOM_SCORE_ADJ) {
            return bad(format!(
                "`oom_score_adj` must be between 0 and {MAX_OOM_SCORE_ADJ}"
            ));
        }
        if self.timeout_ms == Some(0) {
            return bad("`timeout_ms` must be at least 1; omit it for no bound".into());
        }
        if let Some(cap) = &self.output_cap {
            if cap.max_bytes == 0 {
                return bad("`output_cap.max_bytes` must be at least 1".into());
            }
            if cap.on_overflow == Overflow::Tail && cap.max_bytes > MAX_TAIL_CAP_BYTES {
                return bad(format!(
                    "`output_cap.max_bytes` with `tail` must be at most {MAX_TAIL_CAP_BYTES}; \
                     compute larger tails from the stream"
                ));
            }
        }
        Ok(())
    }

    /// [`Self::stderr_tail_bytes`] or its default.
    #[must_use]
    pub fn effective_stderr_tail_bytes(&self) -> u32 {
        self.stderr_tail_bytes.unwrap_or(DEFAULT_STDERR_TAIL_BYTES)
    }

    /// [`Self::kill_grace_ms`] or its default.
    #[must_use]
    pub fn effective_kill_grace_ms(&self) -> u64 {
        self.kill_grace_ms.unwrap_or(DEFAULT_KILL_GRACE_MS)
    }

    /// Wrap in a request envelope.
    #[must_use]
    pub fn to_request(&self, id: impl Into<RequestId>) -> RpcRequest {
        RpcRequest::new(id, METHOD_PROCESS_OPEN, Some(to_json(self)))
    }

    /// Decode and validate from a `process.open` request. Any other method is
    /// [`ProtocolError::MethodNotFound`].
    pub fn from_request(req: &RpcRequest) -> Result<Self, ProtocolError> {
        if req.method != METHOD_PROCESS_OPEN {
            return Err(ProtocolError::MethodNotFound(req.method.clone()));
        }
        let p: Self = from_params(req.params.clone().unwrap_or(Value::Null))?;
        p.validate()?;
        Ok(p)
    }
}

/// `process.open` result, sent after the program started. Records follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessOpenAck {
    pub process_id: String,
    /// Guest pid of the program (the leader of its tree).
    pub pid: u32,
}

impl ProcessOpenAck {
    #[must_use]
    pub fn to_value(&self) -> Value {
        to_json(self)
    }

    /// Decode a `process.open` `result` value (unknown fields are accepted).
    pub fn from_value(value: Value) -> Result<Self, ProtocolError> {
        serde_json::from_value(value)
            .map_err(|e| ProtocolError::Internal(format!("malformed `process.open` result: {e}")))
    }
}

/// Which part of a `process.open` could not be started ([`ProtocolError::SpawnFailed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpawnTarget {
    Program,
    Cwd,
}

impl fmt::Display for SpawnTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Program => "program",
            Self::Cwd => "cwd",
        })
    }
}

/// Payload of the `EXIT` record: how the program ended. Always the last record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessExit {
    /// Exit code, or `None` when a signal ended the leader.
    pub code: Option<i32>,
    /// Signal that ended the leader.
    pub signal: Option<i32>,
    /// Last bytes of stderr, lossy UTF-8, cut at a character boundary. Empty with `merge`.
    pub stderr_tail: String,
    pub timed_out: bool,
    pub cancelled: bool,
    /// stdout went over `output_cap.max_bytes`.
    pub overflow: bool,
    /// False: something of the tree could not be stopped. The caller should destroy the sandbox.
    pub tree_clean: bool,
    pub duration_ms: u64,
}

impl ProcessExit {
    /// Encode as a complete `EXIT` record (header and JSON payload).
    ///
    /// The payload always fits [`MAX_EXIT_RECORD_LEN`]: if escaping makes `stderr_tail` too
    /// long, its oldest characters are dropped until it fits.
    #[must_use]
    pub fn encode_record(&self) -> Vec<u8> {
        let mut exit = self.clone();
        loop {
            let payload = serde_json::to_vec(&exit).expect("ProcessExit serializes");
            if payload.len() <= MAX_EXIT_RECORD_LEN {
                return encode_record(RecordKind::Exit, &payload)
                    .expect("EXIT payload is within its limit");
            }
            // A character is at most 6 bytes of JSON (`\u001f`), so dropping this many always
            // makes progress without dropping more than needed by much.
            let excess = payload.len() - MAX_EXIT_RECORD_LEN;
            let drop_chars = excess.div_ceil(6).max(1);
            let cut = exit
                .stderr_tail
                .char_indices()
                .nth(drop_chars)
                .map_or(exit.stderr_tail.len(), |(i, _)| i);
            exit.stderr_tail.drain(..cut);
        }
    }

    /// Decode the payload of an `EXIT` record (unknown fields are accepted).
    pub fn from_payload(payload: &[u8]) -> Result<Self, ProtocolError> {
        serde_json::from_slice(payload)
            .map_err(|e| ProtocolError::Internal(format!("malformed `EXIT` record: {e}")))
    }
}

/// `process.cancel` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessCancelParams {
    /// The `process_id` of a `process.open`.
    pub process_id: String,
    /// Grace between ask and force. Default 5000; 0 forces at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_ms: Option<u64>,
}

impl ProcessCancelParams {
    /// [`Self::grace_ms`] or its default.
    #[must_use]
    pub fn effective_grace_ms(&self) -> u64 {
        self.grace_ms.unwrap_or(DEFAULT_CANCEL_GRACE_MS)
    }
}

/// `process.cancel` result, sent when the supervisor has exited. An unknown or finished id
/// answers `was_running: false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessCancelResult {
    pub was_running: bool,
    /// False: something of the tree could not be stopped.
    pub tree_clean: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Call, RpcMessage, encode_frame, record::RecordChannel, record::RecordDecoder};
    use serde_json::json;

    const ID: &str = "6f1c2a43-8a51-4c43-9d3e-2b0a8e9c1f00";

    fn full() -> ProcessOpenParams {
        ProcessOpenParams {
            process_id: ID.into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            cwd: Some("/workspace".into()),
            stdin: StdinMode::Pipe,
            stderr: StderrMode::Stream,
            stderr_tail_bytes: Some(1024),
            limits: Some(ExecLimits {
                nproc: Some(256),
                data_bytes: Some(MIN_DATA_BYTES),
                fsize_bytes: Some(1 << 30),
                nofile: Some(1024),
            }),
            oom_score_adj: Some(500),
            timeout_ms: Some(60_000),
            kill_grace_ms: Some(100),
            output_cap: Some(OutputCap {
                max_bytes: 1 << 20,
                on_overflow: Overflow::Kill,
            }),
        }
    }

    #[test]
    fn open_round_trips_through_a_frame() {
        for p in [full(), ProcessOpenParams::new(ID, vec!["true".into()])] {
            let frame = encode_frame(&p.to_request(1)).unwrap();
            let RpcMessage::Request(req) = RpcMessage::from_slice(&frame[4..]).unwrap() else {
                panic!("request")
            };
            assert_eq!(ProcessOpenParams::from_request(&req).unwrap(), p);
        }
    }

    #[test]
    fn open_defaults_and_wire_names() {
        let req = RpcRequest::new(
            1,
            METHOD_PROCESS_OPEN,
            Some(json!({"process_id": ID, "argv": ["ls"]})),
        );
        let p = ProcessOpenParams::from_request(&req).unwrap();
        assert_eq!(p, ProcessOpenParams::new(ID, vec!["ls".into()]));
        assert_eq!(p.stdin, StdinMode::Null);
        assert_eq!(p.stderr, StderrMode::Tail);
        assert_eq!(p.effective_stderr_tail_bytes(), 4096);
        assert_eq!(p.effective_kill_grace_ms(), 500);

        let v = to_json(&full());
        assert_eq!(v["stdin"], "pipe");
        assert_eq!(v["stderr"], "stream");
        assert_eq!(
            v["output_cap"],
            json!({"max_bytes": 1 << 20, "on_overflow": "kill"})
        );
        for (mode, s) in [
            (StderrMode::Tail, "tail"),
            (StderrMode::Merge, "merge"),
            (StderrMode::Stream, "stream"),
        ] {
            assert_eq!(to_json(&mode), json!(s));
        }
        assert_eq!(to_json(&Overflow::Tail), json!("tail"));
    }

    #[test]
    fn open_rejects_bad_params() {
        let base = json!({"process_id": ID, "argv": ["ls"]});
        let with = |k: &str, v: Value| {
            let mut o = base.clone();
            o[k] = v;
            o
        };
        for params in [
            with("extra", json!(1)),
            with("process_id", json!("p-1")),
            with("argv", json!([])),
            with("argv", json!([""])),
            with("argv", json!(["a\u{0}b"])),
            with("env", json!({"A=B": "x"})),
            with("env", json!({"": "x"})),
            with("env", json!({"A": "x\u{0}"})),
            with("cwd", json!("relative")),
            with("stdin", json!("tty")),
            with("stderr", json!("drop")),
            with("stderr_tail_bytes", json!(65537)),
            with("limits", json!({"data_bytes": 1024})),
            with("limits", json!({"nofile": 3})),
            with("limits", json!({"cpu": 1})),
            with("oom_score_adj", json!(1001)),
            with("oom_score_adj", json!(-1)),
            with("timeout_ms", json!(0)),
            with("output_cap", json!({"max_bytes": 0, "on_overflow": "kill"})),
            with(
                "output_cap",
                json!({"max_bytes": MAX_TAIL_CAP_BYTES + 1, "on_overflow": "tail"}),
            ),
            with("output_cap", json!({"max_bytes": 1, "on_overflow": "drop"})),
            json!({"argv": ["ls"]}),
            json!(null),
        ] {
            let req = RpcRequest::new(1, METHOD_PROCESS_OPEN, Some(params.clone()));
            assert!(
                matches!(
                    ProcessOpenParams::from_request(&req),
                    Err(ProtocolError::InvalidParams(_))
                ),
                "{params}"
            );
        }
        // The kill cap has no 8 MiB bound; the edges of the other bounds are accepted.
        for params in [
            with(
                "output_cap",
                json!({"max_bytes": MAX_TAIL_CAP_BYTES + 1, "on_overflow": "kill"}),
            ),
            with(
                "output_cap",
                json!({"max_bytes": MAX_TAIL_CAP_BYTES, "on_overflow": "tail"}),
            ),
            with("stderr_tail_bytes", json!(65536)),
            with("oom_score_adj", json!(1000)),
            with(
                "limits",
                json!({"nofile": 64, "data_bytes": MIN_DATA_BYTES}),
            ),
        ] {
            let req = RpcRequest::new(1, METHOD_PROCESS_OPEN, Some(params.clone()));
            ProcessOpenParams::from_request(&req).unwrap_or_else(|e| panic!("{params}: {e}"));
        }
    }

    #[test]
    fn env_values_never_appear_in_errors() {
        let mut p = ProcessOpenParams::new(ID, vec!["x".into()]);
        p.env.insert("TOKEN".into(), "s3cret\0".into());
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains("TOKEN") && !err.contains("s3cret"), "{err}");
    }

    #[test]
    fn open_is_not_a_tool_or_call() {
        assert!(!crate::TOOL_NAMES.contains(&METHOD_PROCESS_OPEN));
        assert!(matches!(
            Call::from_method_params(METHOD_PROCESS_OPEN, to_json(&full())),
            Err(ProtocolError::MethodNotFound(_))
        ));
        let req = RpcRequest::new(1, "bash", Some(json!({})));
        assert!(matches!(
            ProcessOpenParams::from_request(&req),
            Err(ProtocolError::MethodNotFound(_))
        ));
    }

    #[test]
    fn ack_round_trip_accepts_unknown_fields() {
        let ack = ProcessOpenAck {
            process_id: ID.into(),
            pid: 42,
        };
        assert_eq!(ProcessOpenAck::from_value(ack.to_value()).unwrap(), ack);
        let mut v = ack.to_value();
        v["extra"] = json!(true);
        assert_eq!(ProcessOpenAck::from_value(v).unwrap(), ack);
        assert!(matches!(
            ProcessOpenAck::from_value(json!({"pid": 1})),
            Err(ProtocolError::Internal(_))
        ));
    }

    fn exit() -> ProcessExit {
        ProcessExit {
            code: Some(2),
            signal: None,
            stderr_tail: "boom\n".into(),
            timed_out: false,
            cancelled: false,
            overflow: true,
            tree_clean: true,
            duration_ms: 12,
        }
    }

    #[test]
    fn exit_record_round_trip() {
        let rec = exit().encode_record();
        assert_eq!(rec[0], 0x1F);
        let mut dec = RecordDecoder::new(RecordChannel::ProcessOutput);
        dec.push(&rec);
        let r = dec.next_record().unwrap().unwrap();
        assert_eq!(r.kind, RecordKind::Exit);
        assert_eq!(ProcessExit::from_payload(&r.payload).unwrap(), exit());
        let v: Value = serde_json::from_slice(&r.payload).unwrap();
        assert_eq!(v["signal"], Value::Null);

        let mut extra = v.clone();
        extra["future"] = json!(1);
        assert_eq!(
            ProcessExit::from_payload(&serde_json::to_vec(&extra).unwrap()).unwrap(),
            exit()
        );
        assert!(ProcessExit::from_payload(b"{}").is_err());
    }

    #[test]
    fn exit_record_trims_an_escaped_tail_to_fit() {
        // 64 KiB of control bytes escape to 6 bytes each: far over the 256 KiB EXIT limit.
        let mut e = exit();
        e.stderr_tail = "\u{1}".repeat(MAX_STDERR_TAIL_BYTES as usize - 3) + "end";
        let rec = e.encode_record();
        assert!(rec.len() - crate::record::RECORD_HEADER_LEN <= MAX_EXIT_RECORD_LEN);
        let back = ProcessExit::from_payload(&rec[crate::record::RECORD_HEADER_LEN..]).unwrap();
        assert!(back.stderr_tail.ends_with("end"));
        assert!(back.stderr_tail.len() > 40_000, "trimmed too much");
        assert_eq!(
            ProcessExit {
                stderr_tail: e.stderr_tail.clone(),
                ..back.clone()
            },
            e
        );

        // Multi-byte characters are cut at a boundary.
        e.stderr_tail = "é".repeat(200_000);
        let rec = e.encode_record();
        let back = ProcessExit::from_payload(&rec[crate::record::RECORD_HEADER_LEN..]).unwrap();
        assert!(back.stderr_tail.chars().all(|c| c == 'é'));
    }

    #[test]
    fn cancel_round_trip_and_defaults() {
        let p: ProcessCancelParams = from_params(json!({"process_id": ID})).unwrap();
        assert_eq!(p.effective_grace_ms(), DEFAULT_CANCEL_GRACE_MS);
        let p0: ProcessCancelParams =
            from_params(json!({"process_id": ID, "grace_ms": 0})).unwrap();
        assert_eq!(p0.effective_grace_ms(), 0);
        assert!(from_params::<ProcessCancelParams>(json!({"process_id": ID, "x": 1})).is_err());
        let r = ProcessCancelResult {
            was_running: true,
            tree_clean: false,
        };
        assert_eq!(
            serde_json::from_value::<ProcessCancelResult>(to_json(&r)).unwrap(),
            r
        );
    }
}
