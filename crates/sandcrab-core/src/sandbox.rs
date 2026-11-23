use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use sandcrab_protocol::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{MountMode, ResourceLimits, Result, WorkspaceMount, WorkspaceRef};

/// Unique identifier for an ephemeral sandbox instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxId(pub Uuid);

impl SandboxId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SandboxId {
    fn default() -> Self {
        Self::new()
    }
}

/// Caller-facing configuration for spawning an ephemeral sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub template: String,
    pub mounts: Vec<WorkspaceMount>,
    pub default_cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub timeout_secs: Option<u64>,
    pub resources: Option<ResourceLimits>,
}

impl SandboxConfig {
    #[must_use]
    pub fn from_template(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            mounts: Vec::new(),
            default_cwd: None,
            env: BTreeMap::new(),
            timeout_secs: Some(300),
            resources: None,
        }
    }

    #[must_use]
    pub fn mount(
        mut self,
        workspace: impl Into<WorkspaceRef>,
        guest_path: impl Into<PathBuf>,
        mode: MountMode,
    ) -> Self {
        self.mounts.push(WorkspaceMount {
            workspace: workspace.into(),
            guest_path: guest_path.into(),
            mode,
        });
        self
    }

    #[must_use]
    pub fn default_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.default_cwd = Some(cwd.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }
}

/// Resolved launch specification passed to a [`crate::RuntimeBackend`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub id: SandboxId,
    pub config: SandboxConfig,
}

/// Live handle to a running sandbox backed by `crabd`.
#[async_trait]
pub trait SandboxHandle: Send + Sync {
    fn id(&self) -> SandboxId;
    async fn call_tool(&self, call: ToolCall) -> Result<ToolResult>;
    async fn destroy(self: Box<Self>) -> Result<()>;
}
