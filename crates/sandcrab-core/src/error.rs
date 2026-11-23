use sandcrab_protocol::ProtocolError;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, SandboxError>;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("template error: {0}")]
    Template(String),

    #[error("workspace error: {0}")]
    Workspace(String),

    #[error("backend `{backend}` failed: {message}")]
    Backend { backend: String, message: String },

    #[error("insecure backend rejected: {0}")]
    InsecureBackendRejected(String),

    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),
}
