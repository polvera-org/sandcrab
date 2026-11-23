//! Wire protocol, tool parameter/result contracts, and JSON Schema definitions
//! shared between the Sandcrab host control plane and the `crabd` guest daemon.

use std::collections::BTreeMap;

use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Unique identifier for a single RPC invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct RequestId(pub Uuid);

impl RequestId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// Host -> `crabd` RPC request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: RequestId,
    pub call: ToolCall,
}

/// `crabd` -> Host RPC response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: RequestId,
    pub result: Result<ToolOutput, ProtocolError>,
}

/// Errors produced at the protocol or guest tool execution boundary.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum ProtocolError {
    #[error("invalid tool parameters: {0}")]
    InvalidParams(String),

    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),

    #[error("command timed out after {timeout_ms}ms")]
    TimedOut { timeout_ms: u64 },

    #[error("path not allowed: {0}")]
    PathNotAllowed(String),

    #[error("tool not implemented yet: {0}")]
    Unimplemented(String),
}

/// Structured tool result returned by `crabd`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutput {
    pub data: serde_json::Value,
    #[serde(default)]
    pub truncated: bool,
}

/// JSON Schema metadata exported for LLM tool calling and MCP registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: RootSchema,
}

/// All built-in computer-use tools supported by `crabd`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "tool", content = "params", rename_all = "snake_case")]
pub enum ToolCall {
    Bash(BashParams),
    FileRead(FileReadParams),
    FileWrite(FileWriteParams),
    FileEdit(FileEditParams),
    FileDelete(FileDeleteParams),
    FileMove(FileMoveParams),
    ListDir(ListDirParams),
    Glob(GlobParams),
    Grep(GrepParams),
    ProcessStart(ProcessStartParams),
    ProcessPoll(ProcessPollParams),
    ProcessStdin(ProcessStdinParams),
    ProcessKill(ProcessKillParams),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BashParams {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileReadParams {
    pub path: String,
    /// 1-based starting line number.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to return.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileWriteParams {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub append: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileEditParams {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileDeleteParams {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileMoveParams {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListDirParams {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GlobParams {
    pub pattern: String,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GrepParams {
    pub pattern: String,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub context_lines: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessStartParams {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub pty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessPollParams {
    pub process_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessStdinParams {
    pub process_id: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessKillParams {
    pub process_id: String,
}

const fn default_true() -> bool {
    true
}

/// Returns JSON Schema definitions for all built-in Tier 1 & Tier 2 tools.
#[must_use]
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "bash",
            description: "Execute a shell command inside the sandbox.",
            input_schema: schema_for!(BashParams),
        },
        ToolDefinition {
            name: "file_read",
            description: "Read a file with optional 1-based line offset and limit.",
            input_schema: schema_for!(FileReadParams),
        },
        ToolDefinition {
            name: "file_write",
            description: "Create, overwrite, or append to a file inside the sandbox.",
            input_schema: schema_for!(FileWriteParams),
        },
        ToolDefinition {
            name: "file_edit",
            description: "Apply an exact-match search-and-replace edit to a file.",
            input_schema: schema_for!(FileEditParams),
        },
        ToolDefinition {
            name: "file_delete",
            description: "Delete a file or directory inside the sandbox.",
            input_schema: schema_for!(FileDeleteParams),
        },
        ToolDefinition {
            name: "file_move",
            description: "Move or rename a file or directory inside the sandbox.",
            input_schema: schema_for!(FileMoveParams),
        },
        ToolDefinition {
            name: "list_dir",
            description: "List directory entries with metadata.",
            input_schema: schema_for!(ListDirParams),
        },
        ToolDefinition {
            name: "glob",
            description: "Find files matching a glob pattern.",
            input_schema: schema_for!(GlobParams),
        },
        ToolDefinition {
            name: "grep",
            description: "Search file contents using regex or literal patterns.",
            input_schema: schema_for!(GrepParams),
        },
        ToolDefinition {
            name: "process_start",
            description: "Start a managed background or PTY process.",
            input_schema: schema_for!(ProcessStartParams),
        },
        ToolDefinition {
            name: "process_poll",
            description: "Poll status and buffered output of a managed process.",
            input_schema: schema_for!(ProcessPollParams),
        },
        ToolDefinition {
            name: "process_stdin",
            description: "Write data to a managed process's stdin.",
            input_schema: schema_for!(ProcessStdinParams),
        },
        ToolDefinition {
            name: "process_kill",
            description: "Terminate a managed background process.",
            input_schema: schema_for!(ProcessKillParams),
        },
    ]
}
