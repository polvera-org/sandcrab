//! The `RuntimeBackend` / `SandboxHandle` contract every isolation backend implements.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    Connector, LifecycleTimings, NetworkConfig, PortSpec, PublishedPort, ResolvedMount,
    ResourceLimits, Result, SandboxError, SandboxId, SecretSpec, Template, UserSpec,
};

/// Explicit isolation backend identifier.
/// Sandcrab never silently falls back between backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    Firecracker,
    CloudHypervisor,
    Libkrun,
    /// Shared-kernel container backend (hardened OCI containers). Serialized as `docker`;
    /// the pre-rename name `docker-dev` is still accepted on read.
    #[serde(alias = "docker-dev")]
    Docker,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Firecracker => "firecracker",
            Self::CloudHypervisor => "cloud-hypervisor",
            Self::Libkrun => "libkrun",
            Self::Docker => "docker",
        })
    }
}

/// Capabilities explicitly declared by a [`RuntimeBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    /// Guest runs behind a hardware virtualization boundary (not a shared kernel).
    pub hardware_isolated: bool,
    /// Safe to run mutually untrusted tenants on one host.
    pub multi_tenant_safe: bool,
    pub supports_snapshots: bool,
    /// Live host↔guest directory sharing (bind mounts / virtio-fs). When false, workspaces
    /// are copied in at spawn and synced back at teardown.
    pub supports_host_dir_mounts: bool,
}

/// Fully resolved launch specification handed to [`RuntimeBackend::spawn`].
///
/// Everything here has been validated by the manager: template exists and is valid,
/// mounts are leased and their guest paths safe, the network policy is one the backend
/// claims to enforce, secrets are empty (Phase 3).
#[derive(Debug, Clone)]
pub struct ResolvedSpec {
    pub id: SandboxId,
    pub template: Template,
    /// Template image reference.
    pub image: String,
    /// Pin for the template image on this host (`sha256:<hex>`): the host-local pin, else a
    /// deprecated in-manifest one ([`crate::pins::effective_pin`]); `None` = unpinned. Check
    /// with [`crate::verify_image_pin`]; refuse `None` with [`crate::pins::unpinned_error`].
    pub image_sha256: Option<String>,
    /// Workspace mounts; `host_path` is the canonical path held by the lease.
    pub mounts: Vec<ResolvedMount>,
    pub default_cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub network: NetworkConfig,
    pub ports: Vec<PortSpec>,
    /// Guest user; `None` = backend default (the template image's user).
    pub user: Option<UserSpec>,
    pub resources: ResourceLimits,
    pub ttl: Option<Duration>,
    /// Always empty in V1; backends must still reject non-empty (see [`Self::reject_secrets`]).
    pub secrets: Vec<SecretSpec>,
    /// Shared timing recorder (`spawn_requested` already set). Backends mark
    /// `BackendStarted` and custom sub-phases, and return a clone from
    /// [`SandboxHandle::timings`].
    pub timings: LifecycleTimings,
}

impl ResolvedSpec {
    /// Belt-and-braces check for backends: secrets are not supported in V1.
    pub fn reject_secrets(&self) -> Result<()> {
        if self.secrets.is_empty() {
            Ok(())
        } else {
            Err(SandboxError::PolicyUnenforceable(
                "secrets are reserved for Phase 3 and not supported by this backend".into(),
            ))
        }
    }
}

/// Trait implemented by every sandbox execution backend.
#[async_trait]
pub trait RuntimeBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> BackendCapabilities;

    /// Whether this backend can *enforce* `network`. The manager refuses to spawn when this
    /// returns false; backends must also fail `spawn` themselves — never ignore a policy.
    fn enforces_network_policy(&self, network: &NetworkConfig) -> bool;

    /// Why `network` is not enforced and what would make it work (appended to the manager's
    /// `PolicyUnenforceable` error). `None`: no backend-specific advice.
    fn network_policy_hint(&self, _network: &NetworkConfig) -> Option<String> {
        None
    }

    /// Start a sandbox running `crabd` and return as soon as it can be connected to
    /// (readiness is awaited by the manager via [`SandboxHandle::connector`]).
    /// On error the backend must have cleaned up everything it created.
    async fn spawn(&self, spec: &ResolvedSpec) -> Result<Box<dyn SandboxHandle>>;
}

/// Live handle to a running sandbox.
#[async_trait]
pub trait SandboxHandle: Send + Sync {
    fn id(&self) -> SandboxId;

    /// Opens transport streams to the sandbox's `crabd` tools endpoint.
    fn connector(&self) -> Arc<dyn Connector>;

    /// Guest ports published on host loopback.
    fn published_ports(&self) -> Vec<PublishedPort>;

    /// The timing recorder from the spec this sandbox was spawned with.
    fn timings(&self) -> LifecycleTimings;

    /// Tear down and release every resource (container/VM, sockets, run dir, taps, ...),
    /// syncing rw workspaces back first where the backend copies them. The manager marks
    /// `TeardownFinished` after this returns.
    async fn destroy(self: Box<Self>) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::BackendKind;

    #[test]
    fn docker_kind_serializes_as_docker_and_accepts_legacy_name() {
        assert_eq!(BackendKind::Docker.to_string(), "docker");
        assert_eq!(
            serde_json::to_string(&BackendKind::Docker).unwrap(),
            "\"docker\""
        );
        for name in ["\"docker\"", "\"docker-dev\""] {
            assert_eq!(
                serde_json::from_str::<BackendKind>(name).unwrap(),
                BackendKind::Docker
            );
        }
    }
}
