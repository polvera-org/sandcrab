use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Result, SandboxHandle, SandboxId, SandboxSpec};

/// Explicit isolation backend identifier.
/// Sandcrab never silently falls back between backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    Firecracker,
    CloudHypervisor,
    Libkrun,
    /// Shared-kernel container backend strictly for local dev & CI.
    DockerDev,
}

/// Capabilities explicitly declared by a [`RuntimeBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub hardware_isolated: bool,
    pub multi_tenant_safe: bool,
    pub supports_snapshots: bool,
    pub supports_host_dir_mounts: bool,
}

/// Trait implemented by every sandbox execution backend.
#[async_trait]
pub trait RuntimeBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> BackendCapabilities;

    async fn spawn(&self, spec: &SandboxSpec) -> Result<Box<dyn SandboxHandle>>;
    async fn destroy(&self, id: &SandboxId) -> Result<()>;
}
