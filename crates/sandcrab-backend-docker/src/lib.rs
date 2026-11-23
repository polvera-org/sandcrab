//! Docker/OCI local development and CI backend for Sandcrab.
//!
//! **Security note**: Containers share the host kernel. This backend explicitly reports
//! `hardware_isolated = false` and `multi_tenant_safe = false` so callers can never
//! accidentally treat it as a multi-tenant production isolation boundary.

use async_trait::async_trait;
use sandcrab_core::{
    BackendCapabilities, BackendKind, Result, RuntimeBackend, SandboxError, SandboxHandle,
    SandboxId, SandboxSpec,
};

/// Configuration for [`DockerDevBackend`].
#[derive(Debug, Clone)]
pub struct DockerDevConfig {
    /// Explicit acknowledgement required before spawning shared-kernel dev containers.
    pub allow_insecure_dev_backend: bool,
    /// Optional Docker socket URI override (defaults to local platform socket).
    pub docker_host: Option<String>,
}

impl Default for DockerDevConfig {
    fn default() -> Self {
        Self {
            allow_insecure_dev_backend: true,
            docker_host: None,
        }
    }
}

/// Phase 1 local development backend powered by Docker + `crabd`.
#[derive(Debug, Clone)]
pub struct DockerDevBackend {
    config: DockerDevConfig,
}

impl DockerDevBackend {
    pub fn new(config: DockerDevConfig) -> Result<Self> {
        if !config.allow_insecure_dev_backend {
            return Err(SandboxError::InsecureBackendRejected(
                "DockerDevBackend requires explicit `allow_insecure_dev_backend = true`".into(),
            ));
        }
        Ok(Self { config })
    }

    #[must_use]
    pub fn config(&self) -> &DockerDevConfig {
        &self.config
    }
}

#[async_trait]
impl RuntimeBackend for DockerDevBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::DockerDev
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            hardware_isolated: false,
            multi_tenant_safe: false,
            supports_snapshots: false,
            supports_host_dir_mounts: true,
        }
    }

    async fn spawn(&self, _spec: &SandboxSpec) -> Result<Box<dyn SandboxHandle>> {
        Err(SandboxError::Unimplemented(
            "DockerDevBackend::spawn (Phase 1D: bollard container + workspace bind mounts + crabd RPC)",
        ))
    }

    async fn destroy(&self, _id: &SandboxId) -> Result<()> {
        Err(SandboxError::Unimplemented(
            "DockerDevBackend::destroy (Phase 1D)",
        ))
    }
}
