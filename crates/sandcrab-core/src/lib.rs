//! Core abstractions for Sandcrab: runtime backends, persistent workspaces, templates,
//! the multiplexed `crabd` client, and the sandbox lifecycle manager.
//!
//! Flow: [`SandboxManager::spawn`] validates a [`SandboxConfig`], applies the capability and
//! policy gates (no silent fallback between backends, no ignored network policy),
//! resolves the [`Template`] and workspace mounts ([`resolve_mounts`]), leases workspaces
//! from the [`WorkspaceRegistry`], calls [`RuntimeBackend::spawn`] with a [`ResolvedSpec`],
//! waits for `crabd` ([`wait_ready`]) and returns a [`Sandbox`] tool-call proxy.

pub mod archive;
pub mod backend;
pub mod client;
pub mod error;
pub(crate) mod fsx;
pub mod manager;
pub mod mounts;
pub mod policy;
pub mod registry;
pub mod sandbox;
pub mod template;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;
pub mod timings;
pub mod workspace;

pub use backend::{BackendCapabilities, BackendKind, ResolvedSpec, RuntimeBackend, SandboxHandle};
pub use client::{
    AsyncStream, BoxedStream, CALL_TIMEOUT_GRACE, Connector, CrabdClient, DEFAULT_CALL_TIMEOUT,
    Endpoint, ReadyClient, effective_timeout, wait_ready,
};
pub use error::{Result, SandboxError};
pub use manager::{ManagerConfig, Sandbox, SandboxInfo, SandboxManager};
pub use mounts::{
    FORBIDDEN_GUEST_PREFIXES, ResolvedMount, ResolvedMounts, normalize_guest_path, resolve_mounts,
    validate_mount_target,
};
pub use policy::{
    NetworkConfig, NetworkPolicy, PortSpec, PublishedPort, ResourceOverrides, SecretSource,
    SecretSpec, UserSpec,
};
pub use registry::{
    DEFAULT_READ_LIMIT, FileEntry, FileKind, HostWriteOptions, STATE_ROOT_ENV, WorkspaceLease,
    WorkspaceRegistry, default_state_root, verify_workspace_root,
};
pub use sandbox::{DEFAULT_TTL_SECS, SandboxConfig, SandboxId};
pub use template::{
    ImageArch, ImageVerifier, PinStatus, ResourceLimits, Template, TemplateManifest, TemplateMeta,
    TemplateNetwork, TemplateRegistry, ToolConfig, WorkspaceDefaults, check_digest,
    verify_template_image, verify_template_image_for,
};
pub use timings::{LifecyclePhase, LifecycleTimings, PhaseOffset, TimingMarks, TimingReport};
pub use workspace::{
    MountMode, RootIdentity, SafeRelPath, Workspace, WorkspaceBacking, WorkspaceId, WorkspaceMount,
    WorkspaceRef, WorkspaceState, validate_workspace_name,
};
