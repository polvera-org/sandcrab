//! The `RuntimeBackend` / `SandboxHandle` contract every isolation backend implements.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    Connector, ForwardSpec, LifecycleTimings, NetworkConfig, PortSpec, PublishedPort,
    ResolvedMount, ResourceLimits, Result, SandboxError, SandboxId, SecretSpec, Template, UserSpec,
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

/// What `RLIMIT_NPROC` set on a process inside a sandbox counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NprocScope {
    /// Every process of that uid on the host, in every container: containers share the
    /// kernel's per-uid accounting (Docker). Use `pids_max` to bound a sandbox instead.
    #[default]
    HostUid,
    /// The processes of that uid inside this sandbox's own kernel (microVMs).
    Sandbox,
}

/// Capabilities explicitly declared by a [`RuntimeBackend`].
///
/// The manager refuses a [`crate::SandboxConfig`] that asks for something the backend does
/// not declare here ([`SandboxError::PolicyUnenforceable`]); it never drops the request.
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
    /// Serves [`ResolvedSpec::forwards`] (guest loopback ports forwarded to the host side).
    #[serde(default)]
    pub supports_forwards: bool,
    /// Serves [`ResolvedSpec::host_mounts`] (ad-hoc host directories at the same path).
    #[serde(default)]
    pub supports_host_mounts: bool,
    /// Honours [`ResolvedSpec::tmp_exec`] and [`ResolvedSpec::tmp_size_mb`].
    #[serde(default)]
    pub supports_tmp_options: bool,
    /// Honours [`ResolvedSpec::pids_max`] and [`ResolvedSpec::nofile`].
    #[serde(default)]
    pub supports_process_limits: bool,
    /// Checks the `(dev, ino)` of every host mount inside the started sandbox (design §6.3).
    #[serde(default)]
    pub mount_identity_checked: bool,
    #[serde(default)]
    pub nproc_scope: NprocScope,
}

/// The options added for the server (forwards, host mounts, `/tmp`, process limits) that a
/// spawn asks for; checked against [`BackendCapabilities`] by [`Self::check`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestedOptions {
    pub forwards: bool,
    pub host_mounts: bool,
    pub tmp_options: bool,
    pub process_limits: bool,
}

impl RequestedOptions {
    /// Refuse, with an error that names the option and the backend, anything `caps` does not
    /// declare. Never a silent downgrade.
    pub fn check(self, kind: BackendKind, caps: &BackendCapabilities) -> Result<()> {
        let refused = [
            (self.forwards && !caps.supports_forwards, "forwards"),
            (
                self.host_mounts && !caps.supports_host_mounts,
                "host mounts",
            ),
            (
                self.tmp_options && !caps.supports_tmp_options,
                "/tmp options (tmp_exec, tmp_size_mb)",
            ),
            (
                self.process_limits && !caps.supports_process_limits,
                "process limits (pids_max, nofile)",
            ),
        ];
        match refused.iter().find(|(refused, _)| *refused) {
            Some((_, what)) => Err(SandboxError::PolicyUnenforceable(format!(
                "{what} are not supported by the {kind} backend"
            ))),
            None => Ok(()),
        }
    }
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
    /// Guest loopback ports forwarded to the host side (design §4).
    pub forwards: Vec<ForwardSpec>,
    /// Ad-hoc host directories, checked against the manager's [`crate::MountPolicy`].
    /// `host_path == guest_path`; `root_identity` is the `(dev, ino)` seen when they were
    /// opened. Backends treat them as host-dir workspaces.
    pub host_mounts: Vec<ResolvedMount>,
    /// Drop `noexec` from the guest's `/tmp`.
    pub tmp_exec: bool,
    /// Size of the guest's `/tmp`; `None`: the backend's default.
    pub tmp_size_mb: Option<u64>,
    /// Processes in the sandbox; `None`: the backend's default.
    pub pids_max: Option<u32>,
    /// `RLIMIT_NOFILE` of the sandbox's processes; `None`: the backend's default.
    pub nofile: Option<u64>,
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

    /// The server options this spec asks for.
    #[must_use]
    pub fn requested_options(&self) -> RequestedOptions {
        RequestedOptions {
            forwards: !self.forwards.is_empty(),
            host_mounts: !self.host_mounts.is_empty(),
            tmp_options: self.tmp_exec || self.tmp_size_mb.is_some(),
            process_limits: self.pids_max.is_some() || self.nofile.is_some(),
        }
    }

    /// Belt-and-braces check for backends: refuse any option `caps` does not declare.
    pub fn reject_unsupported(&self, kind: BackendKind, caps: &BackendCapabilities) -> Result<()> {
        self.requested_options().check(kind, caps)
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
    use super::{BackendCapabilities, BackendKind, NprocScope, RequestedOptions};
    use crate::SandboxError;

    #[test]
    fn old_capabilities_still_parse() {
        let caps: BackendCapabilities = serde_json::from_str(
            r#"{"hardware_isolated":true,"multi_tenant_safe":false,
                "supports_snapshots":false,"supports_host_dir_mounts":true}"#,
        )
        .unwrap();
        assert!(!caps.supports_forwards && !caps.supports_host_mounts);
        assert_eq!(caps.nproc_scope, NprocScope::HostUid);
    }

    #[test]
    fn requested_options_are_refused_by_name() {
        let none = BackendCapabilities {
            hardware_isolated: true,
            multi_tenant_safe: false,
            supports_snapshots: false,
            supports_host_dir_mounts: false,
            supports_forwards: false,
            supports_host_mounts: false,
            supports_tmp_options: false,
            supports_process_limits: false,
            mount_identity_checked: false,
            nproc_scope: NprocScope::Sandbox,
        };
        RequestedOptions::default()
            .check(BackendKind::Firecracker, &none)
            .unwrap();
        let forwards = RequestedOptions {
            forwards: true,
            ..Default::default()
        };
        let err = forwards.check(BackendKind::Firecracker, &none).unwrap_err();
        assert!(
            matches!(&err, SandboxError::PolicyUnenforceable(m)
                if m == "forwards are not supported by the firecracker backend"),
            "{err}"
        );
        for (req, word) in [
            (
                RequestedOptions {
                    host_mounts: true,
                    ..Default::default()
                },
                "host mounts",
            ),
            (
                RequestedOptions {
                    tmp_options: true,
                    ..Default::default()
                },
                "tmp_exec",
            ),
            (
                RequestedOptions {
                    process_limits: true,
                    ..Default::default()
                },
                "pids_max",
            ),
        ] {
            let err = req.check(BackendKind::Firecracker, &none).unwrap_err();
            assert!(err.to_string().contains(word), "{err}");
        }
        let all = BackendCapabilities {
            supports_forwards: true,
            supports_host_mounts: true,
            supports_tmp_options: true,
            supports_process_limits: true,
            ..none
        };
        RequestedOptions {
            forwards: true,
            host_mounts: true,
            tmp_options: true,
            process_limits: true,
        }
        .check(BackendKind::Docker, &all)
        .unwrap();
    }

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
