//! Ergonomic async Rust SDK for creating ephemeral Sandcrab sandboxes,
//! managing persistent workspaces, and invoking computer-use tools.

use std::path::PathBuf;
use std::sync::Arc;

use sandcrab_core::{
    BackendCapabilities, BackendKind, ManagerConfig, Result, RuntimeBackend, SandboxConfig,
    SandboxError, SandboxId, SandboxManager, TemplateRegistry, WorkspaceRegistry,
};
use sandcrab_protocol::{
    BashParams, BashResult, FileEditParams, FileEditResult, FileReadParams, FileReadResult,
    FileWriteParams, FileWriteResult, GlobParams, GlobResult, GrepParams, GrepResult, ToolCall,
    ToolDefinition, ToolResult, tool_definitions,
};

/// Builder for configuring a [`Sandcrab`] manager instance.
#[derive(Default)]
pub struct SandcrabBuilder {
    backend: Option<Arc<dyn RuntimeBackend>>,
    state_root: Option<PathBuf>,
    template_dirs: Vec<PathBuf>,
    require_hardware_isolation: bool,
    allow_insecure_backend: bool,
}

impl SandcrabBuilder {
    #[must_use]
    pub fn backend<B: RuntimeBackend + 'static>(mut self, backend: B) -> Self {
        self.backend = Some(Arc::new(backend));
        self
    }

    /// Sandcrab state root (workspace registry); defaults to `~/.sandcrab`.
    #[must_use]
    pub fn workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.state_root = Some(root.into());
        self
    }

    /// Add a directory of templates (`<dir>/<name>/sandcrab.toml`). Earlier dirs win.
    #[must_use]
    pub fn template_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.template_dirs.push(dir.into());
        self
    }

    #[must_use]
    pub fn require_hardware_isolation(mut self, required: bool) -> Self {
        self.require_hardware_isolation = required;
        self
    }

    /// Explicitly allow a backend without hardware isolation (DockerDev, local dev/CI only).
    #[must_use]
    pub fn allow_insecure_backend(mut self, allow: bool) -> Self {
        self.allow_insecure_backend = allow;
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

        let workspaces = match self.state_root {
            Some(root) => WorkspaceRegistry::open(root)?,
            None => WorkspaceRegistry::open_default()?,
        };
        let templates = TemplateRegistry::load(&self.template_dirs)?;
        let manager = SandboxManager::new(
            backend,
            workspaces,
            templates,
            ManagerConfig {
                allow_insecure_backend: self.allow_insecure_backend,
                ..ManagerConfig::default()
            },
        )?;
        Ok(Sandcrab {
            manager,
            require_hardware_isolation: self.require_hardware_isolation,
        })
    }
}

/// Top-level Sandcrab sandbox & workspace manager.
pub struct Sandcrab {
    manager: SandboxManager,
    require_hardware_isolation: bool,
}

impl Sandcrab {
    #[must_use]
    pub fn builder() -> SandcrabBuilder {
        SandcrabBuilder::default()
    }

    #[must_use]
    pub fn backend_kind(&self) -> BackendKind {
        self.manager.backend_kind()
    }

    #[must_use]
    pub fn backend_capabilities(&self) -> BackendCapabilities {
        self.manager.backend_capabilities()
    }

    #[must_use]
    pub fn workspace_root(&self) -> &std::path::Path {
        self.manager.workspaces().state_root()
    }

    #[must_use]
    pub fn manager(&self) -> &SandboxManager {
        &self.manager
    }

    /// Spawn a new ephemeral sandbox from `config`.
    pub async fn create(&self, mut config: SandboxConfig) -> Result<Sandbox> {
        config.require_hardware_isolation |= self.require_hardware_isolation;
        let inner = self.manager.spawn(config).await?;
        Ok(Sandbox { inner })
    }
}

/// Live handle to a single ephemeral sandbox.
pub struct Sandbox {
    inner: sandcrab_core::Sandbox,
}

impl Sandbox {
    #[must_use]
    pub fn id(&self) -> SandboxId {
        self.inner.id()
    }

    /// Export JSON Schemas for all built-in tools (for LLM tool-calling / MCP).
    #[must_use]
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        tool_definitions()
    }

    pub async fn call_tool(&self, call: ToolCall) -> Result<ToolResult> {
        self.inner.call_tool(call).await
    }

    /// Call a tool and extract its typed result.
    async fn call_typed<R>(&self, call: ToolCall) -> Result<R>
    where
        R: TryFrom<ToolResult, Error = sandcrab_protocol::ProtocolError>,
    {
        Ok(R::try_from(self.call_tool(call).await?)?)
    }

    pub async fn bash(&self, command: impl Into<String>) -> Result<BashResult> {
        self.call_typed(ToolCall::Bash(BashParams {
            command: command.into(),
            cwd: None,
            env: Default::default(),
            timeout_ms: None,
        }))
        .await
    }

    pub async fn file_read(&self, path: impl Into<String>) -> Result<FileReadResult> {
        self.call_typed(ToolCall::FileRead(FileReadParams {
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
    ) -> Result<FileWriteResult> {
        self.call_typed(ToolCall::FileWrite(FileWriteParams {
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
    ) -> Result<FileEditResult> {
        self.call_typed(ToolCall::FileEdit(FileEditParams {
            path: path.into(),
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }))
        .await
    }

    pub async fn glob(&self, pattern: impl Into<String>) -> Result<GlobResult> {
        self.call_typed(ToolCall::Glob(GlobParams {
            pattern: pattern.into(),
            root: None,
            respect_gitignore: true,
        }))
        .await
    }

    pub async fn grep(&self, pattern: impl Into<String>) -> Result<GrepResult> {
        self.call_typed(ToolCall::Grep(GrepParams {
            pattern: pattern.into(),
            root: None,
            glob: None,
            literal: false,
            case_insensitive: false,
            context_lines: None,
        }))
        .await
    }

    pub async fn destroy(self) -> Result<()> {
        self.inner.destroy().await.map(|_| ())
    }
}
