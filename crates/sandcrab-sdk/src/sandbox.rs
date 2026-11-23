//! [`Sandbox`]: typed helpers for every tool in the catalog.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sandcrab_core::{
    BackendKind, PublishedPort, Result, SandboxError, SandboxId, SandboxInfo, TimingMarks,
};
use sandcrab_protocol::{
    BashParams, BashResult, DownloadParams, DownloadResult, FileDeleteParams, FileDeleteResult,
    FileEditParams, FileEditResult, FileMoveParams, FileMoveResult, FileReadParams, FileReadResult,
    FileWriteParams, FileWriteResult, GlobParams, GlobResult, GrepParams, GrepResult, HelloResult,
    KillSignal, ListDirParams, ListDirResult, ProcessKillParams, ProcessKillResult,
    ProcessListParams, ProcessListResult, ProcessPollParams, ProcessPollResult, ProcessStartParams,
    ProcessStartResult, ProcessStdinParams, ProcessStdinResult, ProtocolError, ToolCall,
    ToolDefinition, ToolResult, UploadParams, UploadResult, tool_definitions,
};

/// A live sandbox. Every tool runs inside it via `crabd`; relative paths resolve against
/// the sandbox's default working directory.
///
/// Not `Clone`: wrap it in an `Arc` to share. Call [`Sandbox::destroy`] when done — see the
/// crate docs for what happens on drop.
pub struct Sandbox {
    inner: sandcrab_core::Sandbox,
    detached: bool,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.detached || self.inner.is_destroyed() {
            return;
        }
        tracing::warn!(
            id = %self.inner.id(),
            "Sandbox dropped without destroy(); destroying it in the background \
             (falls back to the TTL reaper if no tokio runtime is available)"
        );
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let inner = self.inner.clone();
            rt.spawn(async move {
                if let Err(e) = inner.destroy().await {
                    tracing::warn!(id = %inner.id(), error = %e, "background destroy failed");
                }
            });
        }
    }
}

impl Sandbox {
    pub(crate) fn new(inner: sandcrab_core::Sandbox) -> Self {
        Self {
            inner,
            detached: false,
        }
    }

    #[must_use]
    pub fn id(&self) -> SandboxId {
        self.inner.id()
    }

    #[must_use]
    pub fn template(&self) -> &str {
        self.inner.template()
    }

    #[must_use]
    pub fn backend(&self) -> BackendKind {
        self.inner.backend()
    }

    /// Default working directory inside the sandbox.
    #[must_use]
    pub fn default_cwd(&self) -> &Path {
        self.inner.default_cwd()
    }

    /// Guest ports published on host loopback (`SandboxConfig::port`).
    #[must_use]
    pub fn published_ports(&self) -> &[PublishedPort] {
        self.inner.published_ports()
    }

    /// Host address for a published guest port.
    #[must_use]
    pub fn host_addr(&self, guest_port: u16) -> Option<std::net::SocketAddr> {
        self.published_ports()
            .iter()
            .find(|p| p.guest == guest_port)
            .map(|p| p.host)
    }

    /// Lifecycle timing marks so far (spawn→ready, first call, teardown).
    #[must_use]
    pub fn timings(&self) -> TimingMarks {
        self.inner.timings().snapshot()
    }

    /// `crabd`'s handshake reply (version, tools, default cwd).
    #[must_use]
    pub fn hello(&self) -> &HelloResult {
        self.inner.hello()
    }

    #[must_use]
    pub fn info(&self) -> SandboxInfo {
        self.inner.info()
    }

    #[must_use]
    pub fn is_destroyed(&self) -> bool {
        self.inner.is_destroyed()
    }

    /// The underlying core handle (cheap clone).
    #[must_use]
    pub fn core(&self) -> &sandcrab_core::Sandbox {
        &self.inner
    }

    /// Give up ownership without destroying or warning: the sandbox keeps running until
    /// destroyed through the manager, by its TTL, or when the `Sandcrab` is dropped.
    #[must_use]
    pub fn detach(mut self) -> sandcrab_core::Sandbox {
        self.detached = true;
        self.inner.clone()
    }

    /// LLM / MCP tool definitions (same names as [`Self::tool_dispatch`] accepts).
    #[must_use]
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        tool_definitions()
    }

    /// Tear down (idempotent); returns the final timing marks.
    pub async fn destroy(self) -> Result<TimingMarks> {
        self.inner.destroy().await
    }

    // ---- generic calls -------------------------------------------------------------

    /// Invoke any tool, untyped result.
    pub async fn call(&self, call: impl Into<ToolCall>) -> Result<ToolResult> {
        self.inner.call_tool(call.into()).await
    }

    /// Invoke a tool with its params type and get its typed result, e.g.
    /// `sb.call_typed::<GlobResult>(GlobParams { .. })`.
    pub async fn call_typed<R>(&self, call: impl Into<ToolCall>) -> Result<R>
    where
        R: TryFrom<ToolResult, Error = ProtocolError>,
    {
        self.inner.call(call).await
    }

    // ---- Tier 1 ------------------------------------------------------------------

    /// `bash -c <command>` in the default cwd with the default timeout.
    pub async fn bash(&self, command: impl Into<String>) -> Result<BashResult> {
        self.bash_with(BashParams {
            command: command.into(),
            cwd: None,
            env: Default::default(),
            timeout_ms: None,
        })
        .await
    }

    pub async fn bash_with(&self, params: BashParams) -> Result<BashResult> {
        self.call_typed(params).await
    }

    /// Read a text file (line-numbered, default page size).
    pub async fn file_read(&self, path: impl Into<String>) -> Result<FileReadResult> {
        self.file_read_with(FileReadParams {
            path: path.into(),
            offset: None,
            limit: None,
        })
        .await
    }

    pub async fn file_read_with(&self, params: FileReadParams) -> Result<FileReadResult> {
        self.call_typed(params).await
    }

    /// Create or overwrite a text file.
    pub async fn file_write(
        &self,
        path: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<FileWriteResult> {
        self.file_write_with(FileWriteParams {
            path: path.into(),
            content: content.into(),
            append: false,
        })
        .await
    }

    pub async fn file_write_with(&self, params: FileWriteParams) -> Result<FileWriteResult> {
        self.call_typed(params).await
    }

    /// Replace a unique exact string.
    pub async fn file_edit(
        &self,
        path: impl Into<String>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> Result<FileEditResult> {
        self.file_edit_with(FileEditParams {
            path: path.into(),
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        })
        .await
    }

    pub async fn file_edit_with(&self, params: FileEditParams) -> Result<FileEditResult> {
        self.call_typed(params).await
    }

    /// Delete a file or symlink; directories need `recursive`.
    pub async fn file_delete(
        &self,
        path: impl Into<String>,
        recursive: bool,
    ) -> Result<FileDeleteResult> {
        self.call_typed(FileDeleteParams {
            path: path.into(),
            recursive,
        })
        .await
    }

    pub async fn file_move(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Result<FileMoveResult> {
        self.call_typed(FileMoveParams {
            from: from.into(),
            to: to.into(),
            overwrite,
        })
        .await
    }

    /// List a directory (`None` = default cwd), one level, honoring `.gitignore`.
    pub async fn list_dir(&self, path: Option<&str>) -> Result<ListDirResult> {
        self.list_dir_with(ListDirParams {
            path: path.map(str::to_owned),
            depth: None,
            respect_gitignore: true,
        })
        .await
    }

    pub async fn list_dir_with(&self, params: ListDirParams) -> Result<ListDirResult> {
        self.call_typed(params).await
    }

    pub async fn glob(&self, pattern: impl Into<String>) -> Result<GlobResult> {
        self.glob_with(GlobParams {
            pattern: pattern.into(),
            root: None,
            respect_gitignore: true,
        })
        .await
    }

    pub async fn glob_with(&self, params: GlobParams) -> Result<GlobResult> {
        self.call_typed(params).await
    }

    /// Regex search from the default cwd.
    pub async fn grep(&self, pattern: impl Into<String>) -> Result<GrepResult> {
        self.grep_with(GrepParams {
            pattern: pattern.into(),
            root: None,
            glob: None,
            literal: false,
            case_insensitive: false,
            context_lines: None,
        })
        .await
    }

    pub async fn grep_with(&self, params: GrepParams) -> Result<GrepResult> {
        self.call_typed(params).await
    }

    // ---- Tier 2: background processes ------------------------------------------------

    /// Start `bash -c <command>` in the background (no PTY).
    pub async fn process_start(&self, command: impl Into<String>) -> Result<ProcessStartResult> {
        self.process_start_with(ProcessStartParams {
            command: command.into(),
            cwd: None,
            env: Default::default(),
            pty: false,
        })
        .await
    }

    pub async fn process_start_with(
        &self,
        params: ProcessStartParams,
    ) -> Result<ProcessStartResult> {
        self.call_typed(params).await
    }

    /// Status plus output produced since the previous poll.
    pub async fn process_poll(&self, process_id: impl Into<String>) -> Result<ProcessPollResult> {
        self.call_typed(ProcessPollParams {
            process_id: process_id.into(),
        })
        .await
    }

    pub async fn process_stdin(
        &self,
        process_id: impl Into<String>,
        data: impl Into<String>,
    ) -> Result<ProcessStdinResult> {
        self.call_typed(ProcessStdinParams {
            process_id: process_id.into(),
            data: data.into(),
        })
        .await
    }

    /// Signal the whole process group.
    pub async fn process_kill(
        &self,
        process_id: impl Into<String>,
        signal: KillSignal,
    ) -> Result<ProcessKillResult> {
        self.call_typed(ProcessKillParams {
            process_id: process_id.into(),
            signal,
        })
        .await
    }

    pub async fn process_list(&self) -> Result<ProcessListResult> {
        self.call_typed(ProcessListParams {}).await
    }

    // ---- Tier 2: binary transfer -----------------------------------------------------

    /// Write raw bytes (base64 on the wire). Fails if the file exists unless `overwrite`.
    pub async fn upload(
        &self,
        path: impl Into<String>,
        bytes: impl AsRef<[u8]>,
        overwrite: bool,
    ) -> Result<UploadResult> {
        self.upload_with(UploadParams {
            path: path.into(),
            content_base64: BASE64.encode(bytes),
            mode: None,
            overwrite,
        })
        .await
    }

    pub async fn upload_with(&self, params: UploadParams) -> Result<UploadResult> {
        self.call_typed(params).await
    }

    /// Read a file as base64 (see [`Self::download_bytes`] for decoded bytes).
    pub async fn download(&self, path: impl Into<String>) -> Result<DownloadResult> {
        self.download_with(DownloadParams {
            path: path.into(),
            max_bytes: None,
        })
        .await
    }

    pub async fn download_with(&self, params: DownloadParams) -> Result<DownloadResult> {
        self.call_typed(params).await
    }

    /// Read a file's raw bytes.
    pub async fn download_bytes(&self, path: impl Into<String>) -> Result<Vec<u8>> {
        let res = self.download(path).await?;
        BASE64.decode(res.content_base64.as_bytes()).map_err(|e| {
            SandboxError::Protocol(ProtocolError::Internal(format!(
                "download of `{}` returned invalid base64: {e}",
                res.path
            )))
        })
    }
}
