//! Core abstractions for Sandcrab: runtime backends, persistent workspaces,
//! template manifests, and sandbox lifecycle configuration.

pub mod backend;
pub mod error;
pub mod mounts;
pub mod policy;
pub mod registry;
pub mod sandbox;
pub mod template;
pub mod workspace;

pub use backend::{BackendCapabilities, BackendKind, RuntimeBackend};
pub use error::{Result, SandboxError};
pub use mounts::{
    FORBIDDEN_GUEST_PREFIXES, ResolvedMount, ResolvedMounts, normalize_guest_path, resolve_mounts,
    validate_mount_target,
};
pub use policy::{
    NetworkConfig, NetworkPolicy, PortSpec, PublishedPort, ResourceOverrides, SecretSource,
    SecretSpec, UserSpec,
};
pub use registry::{
    FileEntry, FileKind, STATE_ROOT_ENV, WorkspaceLease, WorkspaceRegistry, default_state_root,
};
pub use sandbox::{DEFAULT_TTL_SECS, SandboxConfig, SandboxHandle, SandboxId, SandboxSpec};
pub use template::{
    ImageVerifier, PinStatus, ResourceLimits, Template, TemplateManifest, TemplateMeta,
    TemplateNetwork, TemplateRegistry, ToolConfig, WorkspaceDefaults, check_digest,
    verify_template_image,
};
pub use workspace::{
    MountMode, SafeRelPath, Workspace, WorkspaceBacking, WorkspaceId, WorkspaceMount, WorkspaceRef,
    WorkspaceState, validate_workspace_name,
};
