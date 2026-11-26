//! Protocol errors and their JSON-RPC 2.0 error-object mapping.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::process::SpawnTarget;

/// JSON-RPC 2.0 standard and Sandcrab application error codes.
pub mod codes {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON is not a valid JSON-RPC request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method (tool) does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameters.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal JSON-RPC / daemon error.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// The tool ran but failed (I/O error, spawn failure, ...).
    pub const EXECUTION_FAILED: i64 = -32001;
    /// The tool exceeded its time budget.
    pub const TIMED_OUT: i64 = -32002;
    /// The path escapes the allowed roots or is otherwise forbidden.
    pub const PATH_NOT_ALLOWED: i64 = -32003;
    /// A file, directory, process or search string was not found.
    pub const NOT_FOUND: i64 = -32004;
    /// `file_edit` matched `old_string` more than once without `replace_all`.
    pub const AMBIGUOUS_EDIT: i64 = -32005;
    /// Input or output exceeds a size limit.
    pub const TOO_LARGE: i64 = -32006;
    /// The tool is not implemented by this daemon / backend.
    pub const UNIMPLEMENTED: i64 = -32007;
    /// The connection did not authenticate (missing, wrong or late `auth` frame).
    pub const UNAUTHORIZED: i64 = -32008;
    /// The peer speaks another protocol version (`hello` from a host of another version).
    pub const UNSUPPORTED_VERSION: i64 = -32009;
    /// `process.open`: the program could not be started. Retrying will not help.
    pub const SPAWN_FAILED: i64 = -32010;
    /// `process.open`: a sandbox-wide limit (e.g. processes open at once) is reached.
    pub const LIMIT_EXCEEDED: i64 = -32011;
    /// `process.open`: a `process.cancel` for this id arrived before the program started.
    pub const CANCELLED: i64 = -32012;
}

/// JSON-RPC 2.0 error object (`{"code", "message", "data"?}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Errors produced at the protocol or guest tool-execution boundary.
///
/// Serializes as a JSON-RPC error object. Messages are written for an LLM reader:
/// they say what went wrong and, where useful, how to fix the call.
#[derive(Debug, Clone, PartialEq, Error, Serialize, Deserialize)]
#[serde(into = "RpcError", from = "RpcError")]
pub enum ProtocolError {
    #[error("parse error: {0}")]
    Parse(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("unknown method `{0}`")]
    MethodNotFound(String),

    #[error("invalid params: {0}")]
    InvalidParams(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    #[error("timed out after {timeout_ms} ms")]
    TimedOut { timeout_ms: u64 },

    #[error("path not allowed: {0}")]
    PathNotAllowed(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error(
        "`old_string` matches {occurrences} times in {path}; include more surrounding context to make it unique, or set `replace_all`"
    )]
    AmbiguousEdit { path: String, occurrences: u64 },

    #[error("too large: {size} bytes exceeds the {limit} byte limit")]
    TooLarge { size: u64, limit: u64 },

    #[error("not implemented: {0}")]
    Unimplemented(String),

    /// The connection was not authenticated; `crabd` closes it after this frame.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The peer speaks another protocol version. `crabd` answers `hello` with this.
    #[error("unsupported protocol version: crabd speaks {crabd}, host speaks {host}")]
    UnsupportedVersion { crabd: String, host: String },

    /// `process.open`: the program could not be started (`errno` such as `ENOENT`, and
    /// whether the `program` or the `cwd` failed). The sandbox works; retrying will not help.
    #[error("could not start the {what}: {errno}")]
    SpawnFailed { errno: String, what: SpawnTarget },

    /// `process.open`: a sandbox-wide limit is reached (`what`, e.g. `processes`).
    #[error("limit exceeded: at most {limit} {what}")]
    LimitExceeded { what: String, limit: u64 },

    /// `process.open`: cancelled by `process.cancel` before the program started.
    #[error("cancelled: {0}")]
    Cancelled(String),

    /// Any error code this crate does not model; preserved verbatim.
    #[error("error {code}: {message}")]
    Other {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

impl ProtocolError {
    /// JSON-RPC error code for this error.
    #[must_use]
    pub fn code(&self) -> i64 {
        match self {
            Self::Parse(_) => codes::PARSE_ERROR,
            Self::InvalidRequest(_) => codes::INVALID_REQUEST,
            Self::MethodNotFound(_) => codes::METHOD_NOT_FOUND,
            Self::InvalidParams(_) => codes::INVALID_PARAMS,
            Self::Internal(_) => codes::INTERNAL_ERROR,
            Self::ExecutionFailed(_) => codes::EXECUTION_FAILED,
            Self::TimedOut { .. } => codes::TIMED_OUT,
            Self::PathNotAllowed(_) => codes::PATH_NOT_ALLOWED,
            Self::NotFound(_) => codes::NOT_FOUND,
            Self::AmbiguousEdit { .. } => codes::AMBIGUOUS_EDIT,
            Self::TooLarge { .. } => codes::TOO_LARGE,
            Self::Unimplemented(_) => codes::UNIMPLEMENTED,
            Self::Unauthorized(_) => codes::UNAUTHORIZED,
            Self::UnsupportedVersion { .. } => codes::UNSUPPORTED_VERSION,
            Self::SpawnFailed { .. } => codes::SPAWN_FAILED,
            Self::LimitExceeded { .. } => codes::LIMIT_EXCEEDED,
            Self::Cancelled(_) => codes::CANCELLED,
            Self::Other { code, .. } => *code,
        }
    }

    /// Convert into a JSON-RPC error object. `data` carries the structured fields.
    #[must_use]
    pub fn to_rpc_error(&self) -> RpcError {
        let data = match self {
            Self::Parse(d)
            | Self::InvalidRequest(d)
            | Self::MethodNotFound(d)
            | Self::InvalidParams(d)
            | Self::Internal(d)
            | Self::ExecutionFailed(d)
            | Self::PathNotAllowed(d)
            | Self::NotFound(d)
            | Self::Unimplemented(d)
            | Self::Unauthorized(d)
            | Self::Cancelled(d) => Some(json!({ "detail": d })),
            Self::UnsupportedVersion { crabd, host } => {
                Some(json!({ "crabd": crabd, "host": host }))
            }
            Self::SpawnFailed { errno, what } => Some(json!({ "errno": errno, "what": what })),
            Self::LimitExceeded { what, limit } => Some(json!({ "what": what, "limit": limit })),
            Self::TimedOut { timeout_ms } => Some(json!({ "timeout_ms": timeout_ms })),
            Self::AmbiguousEdit { path, occurrences } => {
                Some(json!({ "path": path, "occurrences": occurrences }))
            }
            Self::TooLarge { size, limit } => Some(json!({ "size": size, "limit": limit })),
            Self::Other {
                message,
                data,
                code,
            } => {
                return RpcError {
                    code: *code,
                    message: message.clone(),
                    data: data.clone(),
                };
            }
        };
        RpcError {
            code: self.code(),
            message: self.to_string(),
            data,
        }
    }

    /// Rebuild a typed error from a JSON-RPC error object. Unknown codes, or known codes
    /// whose `data` is malformed, become [`ProtocolError::Other`].
    #[must_use]
    pub fn from_rpc_error(err: RpcError) -> Self {
        let data = err.data.as_ref();
        let str_field = |k: &str| data.and_then(|d| d.get(k)).and_then(Value::as_str);
        let u64_field = |k: &str| data.and_then(|d| d.get(k)).and_then(Value::as_u64);
        let detail = || {
            str_field("detail")
                .map(str::to_owned)
                .unwrap_or_else(|| err.message.clone())
        };
        let typed = match err.code {
            codes::PARSE_ERROR => Some(Self::Parse(detail())),
            codes::INVALID_REQUEST => Some(Self::InvalidRequest(detail())),
            codes::METHOD_NOT_FOUND => Some(Self::MethodNotFound(detail())),
            codes::INVALID_PARAMS => Some(Self::InvalidParams(detail())),
            codes::INTERNAL_ERROR => Some(Self::Internal(detail())),
            codes::EXECUTION_FAILED => Some(Self::ExecutionFailed(detail())),
            codes::PATH_NOT_ALLOWED => Some(Self::PathNotAllowed(detail())),
            codes::NOT_FOUND => Some(Self::NotFound(detail())),
            codes::UNIMPLEMENTED => Some(Self::Unimplemented(detail())),
            codes::UNAUTHORIZED => Some(Self::Unauthorized(detail())),
            codes::CANCELLED => Some(Self::Cancelled(detail())),
            codes::UNSUPPORTED_VERSION => {
                str_field("crabd")
                    .zip(str_field("host"))
                    .map(|(crabd, host)| Self::UnsupportedVersion {
                        crabd: crabd.to_owned(),
                        host: host.to_owned(),
                    })
            }
            codes::SPAWN_FAILED => str_field("errno")
                .zip(data.and_then(|d| d.get("what")))
                .and_then(|(errno, what)| {
                    let what = serde_json::from_value(what.clone()).ok()?;
                    Some(Self::SpawnFailed {
                        errno: errno.to_owned(),
                        what,
                    })
                }),
            codes::LIMIT_EXCEEDED => {
                str_field("what")
                    .zip(u64_field("limit"))
                    .map(|(what, limit)| Self::LimitExceeded {
                        what: what.to_owned(),
                        limit,
                    })
            }
            codes::TIMED_OUT => {
                u64_field("timeout_ms").map(|timeout_ms| Self::TimedOut { timeout_ms })
            }
            codes::AMBIGUOUS_EDIT => {
                str_field("path")
                    .zip(u64_field("occurrences"))
                    .map(|(path, occurrences)| Self::AmbiguousEdit {
                        path: path.to_owned(),
                        occurrences,
                    })
            }
            codes::TOO_LARGE => u64_field("size")
                .zip(u64_field("limit"))
                .map(|(size, limit)| Self::TooLarge { size, limit }),
            _ => None,
        };
        typed.unwrap_or(Self::Other {
            code: err.code,
            message: err.message,
            data: err.data,
        })
    }
}

impl From<ProtocolError> for RpcError {
    fn from(err: ProtocolError) -> Self {
        err.to_rpc_error()
    }
}

impl From<RpcError> for ProtocolError {
    fn from(err: RpcError) -> Self {
        Self::from_rpc_error(err)
    }
}
