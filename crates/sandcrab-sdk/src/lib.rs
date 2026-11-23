//! Ergonomic async Rust SDK for creating ephemeral Sandcrab sandboxes,
//! managing persistent workspaces, and invoking computer-use tools.

use std::path::PathBuf;
use std::sync::Arc;

use sandcrab_core::{
    BackendCapabilities, BackendKind, Result, RuntimeBackend, SandboxConfig, SandboxError,
    SandboxHandle, SandboxId, SandboxSpec,
};
use sandcrab_protocol::{
    BashParams, FileEditParams, FileReadParams, FileWriteParams, GlobParams, GrepParams, ToolCall,
    ToolDefinition, ToolOutput, tool_definitions,
};

/// Builder for configuring a [`Sandcrab`] manager instance.
pub struct SandcrabBuilder {
    backend: Option<Arc<dyn RuntimeBackend>>,
    workspace_root: Option<PathBuf>,
    require_hardware_isolation: bool,
}

impl SandcrabBuilder {
    #[must_use]
    pub fn backend<B: RuntimeBackend + 'static>(mut self, backend: B) -> Self {
        self.backend = Some(Arc::new(backend));
        self
    }

    #[must_use]
    pub fn workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    #[must_use]
    pub fn require_hardware_isolation(mut self, required: bool) -> Self {
        self.require_hardware_isolation = required;
        self
    }

    pub fn build(self) -> Result<Sandcrab> {
        let backend = self.backend.ok_or_else(|| SandboxError::Backend {
            backend: "none".into(),
            message: "an explicit RuntimeBackend must be configured".into(),
        })?;

        if self.require_hardware_isolation && !backend.capabilities().hardware_isolated {
            return Err(SandboxError::InsecureBackendRejected(format!(
                "backend {:?} does not provide hardware isolation",
                backend.kind()
            )));
        }

        Ok(Sandcrab {
            backend,
            workspace_root: self
                .workspace_root
                .unwrap_or_else(|| PathBuf::from(".sandcrab/workspaces")),
        })
    }
}

/// Top-level Sandcrab sandbox & workspace manager.
pub struct Sandcrab {
    backend: Arc<dyn RuntimeBackend>,
    workspace_root: PathBuf,
}

impl Sandcrab {
    #[must_use]
    pub fn builder() -> SandcrabBuilder {
        SandcrabBuilder {
            backend: None,
            workspace_root: None,
            require_hardware_isolation: false,
        }
    }

    #[must_use]
    pub fn backend_kind(&self) -> BackendKind {
        self.backend.kind()
    }

    #[must_use]
    pub fn backend_capabilities(&self) -> BackendCapabilities {
        self.backend.capabilities()
    }

    #[must_use]
    pub fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    /// Spawn a new ephemeral sandbox from `config`.
    pub async fn create(&self, config: SandboxConfig) -> Result<Sandbox> {
        let spec = SandboxSpec {
            id: SandboxId::new(),
            config,
        };
        let handle = self.backend.spawn(&spec).await?;
        Ok(Sandbox { handle })
    }
}

/// Live handle to a single ephemeral sandbox.
pub struct Sandbox {
    handle: Box<dyn SandboxHandle>,
}

impl Sandbox {
    #[must_use]
    pub fn id(&self) -> SandboxId {
        self.handle.id()
    }

    /// Export JSON Schemas for all built-in tools (for LLM tool-calling / MCP).
    #[must_use]
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        tool_definitions()
    }

    pub async fn call_tool(&self, call: ToolCall) -> Result<ToolOutput> {
        self.handle.call_tool(call).await
    }

    pub async fn bash(&self, command: impl Into<String>) -> Result<ToolOutput> {
        self.call_tool(ToolCall::Bash(BashParams {
            command: command.into(),
            cwd: None,
            env: Default::default(),
            timeout_ms: None,
        }))
        .await
    }

    pub async fn file_read(&self, path: impl Into<String>) -> Result<ToolOutput> {
        self.call_tool(ToolCall::FileRead(FileReadParams {
            path: path.into(),
            offset: None,
            limit: None,
        }))
        .await
    }

    pub async fn file_write(
        &self,
        path: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<ToolOutput> {
        self.call_tool(ToolCall::FileWrite(FileWriteParams {
            path: path.into(),
            content: content.into(),
            append: false,
        }))
        .await
    }

    pub async fn file_edit(
        &self,
        path: impl Into<String>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> Result<ToolOutput> {
        self.call_tool(ToolCall::FileEdit(FileEditParams {
            path: path.into(),
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }))
        .await
    }

    pub async fn glob(&self, pattern: impl Into<String>) -> Result<ToolOutput> {
        self.call_tool(ToolCall::Glob(GlobParams {
            pattern: pattern.into(),
            root: None,
            respect_gitignore: true,
        }))
        .await
    }

    pub async fn grep(&self, pattern: impl Into<String>) -> Result<ToolOutput> {
        self.call_tool(ToolCall::Grep(GrepParams {
            pattern: pattern.into(),
            root: None,
            glob: None,
            case_insensitive: false,
            context_lines: None,
        }))
        .await
    }

    pub async fn destroy(self) -> Result<()> {
        self.handle.destroy().await
    }
}
