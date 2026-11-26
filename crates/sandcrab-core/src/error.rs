use std::path::{Path, PathBuf};
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

    /// A workspace's root directory is no longer the one registered: a path component is
    /// now a symlink, it is missing, or its `(dev, ino)` changed. Never re-resolved.
    #[error("workspace root changed: {0}")]
    WorkspaceRootChanged(String),

    /// A read exceeded its size limit.
    #[error("too large: {0}")]
    TooLarge(String),

    /// The workspace is leased read-write by a running sandbox, or is in a dirty session.
    #[error("workspace busy: {0}")]
    WorkspaceBusy(String),

    /// A runtime backend failed.
    #[error("backend `{backend}` failed: {message}")]
    Backend { backend: String, message: String },

    /// Hardware isolation was required (`require_hardware_isolation`) but the selected
    /// backend does not provide it. Never answered by switching backends.
    #[error("hardware isolation required: {0}")]
    HardwareIsolationRequired(String),

    /// The backend cannot enforce a requested security policy (network, secrets, ...).
    /// Sandcrab never ignores such a request silently.
    #[error("policy cannot be enforced: {0}")]
    PolicyUnenforceable(String),

    /// A host mount (`SandboxConfig::host_mounts`) was refused: outside the allowed roots,
    /// above a root's mode, a symlink on the way, not a directory, nested, over the cap
    /// (`cap` is then set), or a protected directory.
    #[error("mount `{}` not allowed: {reason}{}", path.display(), cap.map(|c| format!(" (cap: {c})")).unwrap_or_default())]
    MountNotAllowed {
        path: PathBuf,
        reason: String,
        cap: Option<usize>,
    },

    /// The backend or the sandbox could not be reached (daemon down, VM failed, no `crabd`
    /// connection). Nothing ran; trying again may work.
    #[error("unavailable: {0}")]
    Unavailable(String),

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
            Self::WorkspaceRootChanged(s) => Self::WorkspaceRootChanged(s.clone()),
            Self::TooLarge(s) => Self::TooLarge(s.clone()),
            Self::Backend { backend, message } => Self::Backend {
                backend: backend.clone(),
                message: message.clone(),
            },
            Self::HardwareIsolationRequired(s) => Self::HardwareIsolationRequired(s.clone()),
            Self::PolicyUnenforceable(s) => Self::PolicyUnenforceable(s.clone()),
            Self::MountNotAllowed { path, reason, cap } => Self::MountNotAllowed {
                path: path.clone(),
                reason: reason.clone(),
                cap: *cap,
            },
            Self::Unavailable(s) => Self::Unavailable(s.clone()),
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
