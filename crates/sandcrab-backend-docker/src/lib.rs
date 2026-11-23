//! Docker/OCI local development and CI backend for Sandcrab.
//!
//! **Security note**: Containers share the host kernel. This backend explicitly reports
//! `hardware_isolated = false` and `multi_tenant_safe = false` so callers can never
//! accidentally treat it as a multi-tenant production isolation boundary.

mod config;
pub mod spec;

use async_trait::async_trait;
use sandcrab_core::{
    BackendCapabilities, BackendKind, NetworkConfig, NetworkPolicy, ResolvedSpec, Result,
    RuntimeBackend, SandboxError, SandboxHandle,
};

pub use config::{
    CRABD_ENV, DockerDevConfig, DockerLimits, check_static_crabd, default_crabd_path,
    default_state_dir,
};

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

    fn enforces_network_policy(&self, network: &NetworkConfig) -> bool {
        // `--network none` / default bridge; domain allow-lists are not enforceable.
        matches!(
            network.policy,
            NetworkPolicy::Disabled | NetworkPolicy::Unrestricted
        )
    }

    async fn spawn(&self, _spec: &ResolvedSpec) -> Result<Box<dyn SandboxHandle>> {
        Err(SandboxError::Unimplemented(
            "DockerDevBackend::spawn (Phase 1D: bollard container + workspace bind mounts + crabd RPC)",
        ))
    }
}
