//! Core abstractions for Sandcrab: runtime backends, persistent workspaces,
//! template manifests, and sandbox lifecycle configuration.

pub mod backend;
pub mod error;
pub mod registry;
pub mod sandbox;
pub mod template;
pub mod workspace;

pub use backend::{BackendCapabilities, BackendKind, RuntimeBackend};
pub use error::{Result, SandboxError};
pub use registry::{
    FileEntry, FileKind, STATE_ROOT_ENV, WorkspaceLease, WorkspaceRegistry, default_state_root,
};
pub use sandbox::{SandboxConfig, SandboxHandle, SandboxId, SandboxSpec};
pub use template::{
    NetworkConfig, NetworkPolicy, ResourceLimits, TemplateManifest, TemplateMeta, ToolConfig,
    WorkspaceDefaults,
};
pub use workspace::{
    MountMode, SafeRelPath, Workspace, WorkspaceBacking, WorkspaceId, WorkspaceMount, WorkspaceRef,
    WorkspaceState, validate_workspace_name,
};
