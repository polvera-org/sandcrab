//! Core abstractions for Sandcrab: runtime backends, persistent workspaces, templates,
//! the multiplexed `crabd` client, and the sandbox lifecycle manager.
//!
//! Flow: [`SandboxManager::spawn`] validates a [`SandboxConfig`], applies the capability and
//! policy gates (no silent fallback between backends, no ignored network policy),
//! resolves the [`Template`] and workspace mounts ([`resolve_mounts`]), leases workspaces
//! from the [`WorkspaceRegistry`], calls [`RuntimeBackend::spawn`] with a [`ResolvedSpec`],
//! waits for `crabd` ([`wait_ready`]) and returns a [`Sandbox`] tool-call proxy.

pub mod archive;
pub mod auth;
pub mod backend;
pub mod client;
pub mod error;
pub(crate) mod fsx;
pub mod manager;
pub mod mounts;
pub mod pins;
pub mod policy;
pub mod private_dir;
pub mod registry;
pub mod sandbox;
pub mod template;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;
pub mod timings;
pub mod tunnel;
pub mod workspace;

pub use auth::{AuthConnector, DEFAULT_AUTH_HANDSHAKE_TIMEOUT, generate_auth_token};
pub use backend::{BackendCapabilities, BackendKind, ResolvedSpec, RuntimeBackend, SandboxHandle};
pub use client::{
    AsyncStream, BoxedStream, CALL_TIMEOUT_GRACE, Connector, CrabdClient, DEFAULT_CALL_TIMEOUT,
    Endpoint, ReadyClient, effective_timeout, open_tunnel, wait_ready,
};
pub use error::{Result, SandboxError};
pub use manager::{ManagerConfig, Sandbox, SandboxInfo, SandboxManager};
pub use mounts::{
    FORBIDDEN_GUEST_PREFIXES, ResolvedMount, ResolvedMounts, normalize_guest_path, resolve_mounts,
    validate_mount_target,
};
pub use pins::{
    EffectivePin, PINS_DIR_ENV, PinOrigin, PinSource, PinStore, TemplatePin, default_pins_dir,
    effective_pin,
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
    verify_image_pin,
};
pub use timings::{LifecyclePhase, LifecycleTimings, PhaseOffset, TimingMarks, TimingReport};
pub use tunnel::{
    ForwarderConfig, MAX_PUBLISHED_PORTS, PortForwarder, PortForwards, StopSignal, validate_ports,
};
pub use workspace::{
    MountMode, RootIdentity, SafeRelPath, Workspace, WorkspaceBacking, WorkspaceId, WorkspaceMount,
    WorkspaceRef, WorkspaceState, validate_workspace_name,
};

/// Where `scripts/build-crabd.sh` puts the static `crabd` for the host arch:
/// `<target dir>/<arch>-unknown-linux-musl/release/crabd`. The target dir is
/// `$CARGO_TARGET_DIR` when set (a relative value is taken relative to `base`, as cargo takes
/// it relative to its working directory), else `<base>/target`. Pass `Path::new("")` for a
/// path relative to the current directory.
#[must_use]
pub fn crabd_build_path(base: &std::path::Path) -> std::path::PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| base.join("target"), |d| base.join(d));
    target
        .join(format!("{}-unknown-linux-musl", std::env::consts::ARCH))
        .join("release")
        .join("crabd")
}
