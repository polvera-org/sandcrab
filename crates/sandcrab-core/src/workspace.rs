use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a persistent workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(pub Uuid);

impl WorkspaceId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self::new()
    }
}

/// Reference to a workspace by ID or registered name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkspaceRef {
    Id(WorkspaceId),
    Name(String),
}

impl From<WorkspaceId> for WorkspaceRef {
    fn from(id: WorkspaceId) -> Self {
        Self::Id(id)
    }
}

impl From<&str> for WorkspaceRef {
    fn from(name: &str) -> Self {
        Self::Name(name.to_owned())
    }
}

impl From<String> for WorkspaceRef {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

/// Storage backing for a persistent workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum WorkspaceBacking {
    /// User-registered existing host directory.
    HostDir(PathBuf),
    /// Directory managed by Sandcrab under its workspace root.
    ManagedDir(PathBuf),
    /// Persistent ext4 / dm-thin block volume (Firecracker & cloud).
    BlockVolume(PathBuf),
}

/// Mount access mode enforced at the host/VMM boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    ReadWrite,
    ReadOnly,
}

/// A persistent workspace registered with Sandcrab.
/// Workspaces outlive ephemeral sandboxes and may be mutated between turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub backing: WorkspaceBacking,
    pub created_at: DateTime<Utc>,
}

/// Mount instruction binding a workspace into a sandbox at spawn time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMount {
    pub workspace: WorkspaceRef,
    pub guest_path: PathBuf,
    pub mode: MountMode,
}
