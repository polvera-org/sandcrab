use std::path::Path;
use std::time::Duration;

use sandcrab_protocol::ProtocolError;
use thiserror::Error;

pub type Result<T, E = SandboxError> = std::result::Result<T, E>;

/// Every error surfaced by `sandcrab-core`.
///
/// Variants are coarse on purpose: callers (SDK, CLI, MCP server) branch on the kind of
/// failure, and the message carries the detail for humans and LLMs.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// A workspace, template, sandbox or file does not exist.
    #[error("{kind} not found: {id}")]
    NotFound { kind: &'static str, id: String },

    /// A workspace (or other named object) with that name already exists.
    #[error("{kind} already exists: {id}")]
    AlreadyExists { kind: &'static str, id: String },

    /// The caller-supplied configuration is invalid (bad name, path, port, ...).
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// A template manifest failed to parse or validate.
    #[error("template error: {0}")]
    Template(String),

    /// A workspace registry operation failed.
    #[error("workspace error: {0}")]
    Workspace(String),

    /// A host-side path would escape its workspace root (`..`, absolute, or symlink escape).
    #[error("path escapes workspace root: {0}")]
    PathEscape(String),

    /// The workspace is leased read-write by a running sandbox, or is in a dirty session.
    #[error("workspace busy: {0}")]
    WorkspaceBusy(String),

    /// A runtime backend failed.
    #[error("backend `{backend}` failed: {message}")]
    Backend { backend: String, message: String },

    /// An insecure backend was used without explicit opt-in, or hardware isolation was
    /// required but the backend does not provide it.
    #[error("insecure backend rejected: {0}")]
    InsecureBackendRejected(String),

    /// The backend cannot enforce a requested security policy (network, secrets, ...).
    /// Sandcrab never ignores such a request silently.
    #[error("policy cannot be enforced: {0}")]
    PolicyUnenforceable(String),

    /// A pinned image/rootfs digest does not match the artifact actually present.
    #[error("digest mismatch for `{image}`: expected {expected}, got {actual}")]
    DigestMismatch {
        image: String,
        expected: String,
        actual: String,
    },

    /// Host <-> crabd transport failure (connect, read, write, closed connection).
    #[error("transport error: {0}")]
    Transport(String),

    /// An operation exceeded its deadline.
    #[error("timed out after {after:?}: {what}")]
    Timeout { what: String, after: Duration },

    /// crabd answered with a JSON-RPC error, or a frame could not be decoded.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),
}

/// Manual impl because `std::io::Error` is not `Clone`: the copy keeps its kind and message.
/// Needed so every caller awaiting one shared teardown gets the same outcome.
impl Clone for SandboxError {
    fn clone(&self) -> Self {
        match self {
            Self::NotFound { kind, id } => Self::NotFound {
                kind,
                id: id.clone(),
            },
            Self::AlreadyExists { kind, id } => Self::AlreadyExists {
                kind,
                id: id.clone(),
            },
            Self::InvalidConfig(s) => Self::InvalidConfig(s.clone()),
            Self::Template(s) => Self::Template(s.clone()),
            Self::Workspace(s) => Self::Workspace(s.clone()),
            Self::PathEscape(s) => Self::PathEscape(s.clone()),
            Self::WorkspaceBusy(s) => Self::WorkspaceBusy(s.clone()),
            Self::Backend { backend, message } => Self::Backend {
                backend: backend.clone(),
                message: message.clone(),
            },
            Self::InsecureBackendRejected(s) => Self::InsecureBackendRejected(s.clone()),
            Self::PolicyUnenforceable(s) => Self::PolicyUnenforceable(s.clone()),
            Self::DigestMismatch {
                image,
                expected,
                actual,
            } => Self::DigestMismatch {
                image: image.clone(),
                expected: expected.clone(),
                actual: actual.clone(),
            },
            Self::Transport(s) => Self::Transport(s.clone()),
            Self::Timeout { what, after } => Self::Timeout {
                what: what.clone(),
                after: *after,
            },
            Self::Protocol(e) => Self::Protocol(e.clone()),
            Self::Io(e) => Self::Io(std::io::Error::new(e.kind(), e.to_string())),
            Self::Unimplemented(s) => Self::Unimplemented(s),
        }
    }
}

impl SandboxError {
    pub(crate) fn not_found(kind: &'static str, id: impl ToString) -> Self {
        Self::NotFound {
            kind,
            id: id.to_string(),
        }
    }

    /// Wrap an I/O error with the path it happened on.
    pub(crate) fn io_at(path: &Path, err: std::io::Error) -> Self {
        Self::Io(std::io::Error::new(
            err.kind(),
            format!("{}: {err}", path.display()),
        ))
    }
}
