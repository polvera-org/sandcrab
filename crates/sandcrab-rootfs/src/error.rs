use std::path::PathBuf;

/// Errors from the rootfs pipeline and the backends' artifact plumbing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io error: {0}")]
    IoBare(#[from] std::io::Error),
    #[error("`{cmd}` failed ({status}): {stderr}")]
    Command {
        cmd: String,
        status: String,
        stderr: String,
    },
    #[error("sha256 mismatch for {what}: expected {expected}, got {actual}")]
    DigestMismatch {
        what: String,
        expected: String,
        actual: String,
    },
    #[error("image {image} has id {actual}, but the template pins {expected}")]
    ImagePinMismatch {
        image: String,
        expected: String,
        actual: String,
    },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("no usable mke2fs strategy: {0}")]
    NoMkfs(String),
    #[error("invalid manifest: {0}")]
    Manifest(String),
    #[error("neither SANDCRAB_HOME nor HOME is set")]
    NoHome,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Attach the offending path to an `io::Result` error.
pub trait IoCtx<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoCtx<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }
}
