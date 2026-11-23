//! The tool catalog: [`ToolCall`], [`ToolResult`] and their JSON-RPC (method, params) mapping.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use crate::{ProtocolError, ToolDefinition, params::*, results::*, schema};

/// Decode JSON-RPC `params` (absent/`null` is treated as `{}`) into a typed struct.
pub(crate) fn from_params<T: DeserializeOwned>(params: Value) -> Result<T, ProtocolError> {
    let params = match params {
        Value::Null => Value::Object(Map::new()),
        obj @ Value::Object(_) => obj,
        _ => {
            return Err(ProtocolError::InvalidParams(
                "params must be a JSON object".into(),
            ));
        }
    };
    serde_json::from_value(params).map_err(|e| ProtocolError::InvalidParams(e.to_string()))
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("protocol types always serialize to JSON")
}

macro_rules! define_tools {
    ($( $variant:ident => $name:literal, $params:ident, $result:ident, $desc:literal; )+) => {
        /// A typed tool invocation. JSON-RPC `method` = [`ToolCall::name`],
        /// `params` = [`ToolCall::to_params`].
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "tool", content = "params", rename_all = "snake_case")]
        pub enum ToolCall {
            $( #[doc = $desc] $variant($params), )+
        }

        /// A typed tool result. JSON-RPC `result` = [`ToolResult::to_value`].
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "tool", content = "result", rename_all = "snake_case")]
        pub enum ToolResult {
            $( #[doc = $desc] $variant($result), )+
        }

        /// Every tool method name, in catalog order.
        pub const TOOL_NAMES: &[&str] = &[$($name),+];

        impl ToolCall {
            /// JSON-RPC method name of this call.
            #[must_use]
            pub fn name(&self) -> &'static str {
                match self { $( Self::$variant(_) => $name, )+ }
            }

            /// JSON-RPC `params` object of this call.
            #[must_use]
            pub fn to_params(&self) -> Value {
                match self { $( Self::$variant(p) => to_json(p), )+ }
            }

            /// Decode a call from a JSON-RPC method name and params.
            pub fn from_method_params(method: &str, params: Value) -> Result<Self, ProtocolError> {
                match method {
                    $( $name => from_params(params).map(Self::$variant), )+
                    _ => Err(ProtocolError::MethodNotFound(method.to_owned())),
                }
            }
        }

        impl ToolResult {
            /// Tool name this result belongs to.
            #[must_use]
            pub fn name(&self) -> &'static str {
                match self { $( Self::$variant(_) => $name, )+ }
            }

            /// JSON-RPC `result` value.
            #[must_use]
            pub fn to_value(&self) -> Value {
                match self { $( Self::$variant(r) => to_json(r), )+ }
            }

            /// Decode a result given the method of the request it answers.
            pub fn from_method_value(method: &str, value: Value) -> Result<Self, ProtocolError> {
                match method {
                    $( $name => serde_json::from_value(value)
                        .map(Self::$variant)
                        .map_err(|e| ProtocolError::Internal(
                            format!("malformed `{}` result: {e}", $name)
                        )), )+
                    _ => Err(ProtocolError::MethodNotFound(method.to_owned())),
                }
            }
        }

        $(
            impl From<$params> for ToolCall {
                fn from(p: $params) -> Self { Self::$variant(p) }
            }

            impl From<$result> for ToolResult {
                fn from(r: $result) -> Self { Self::$variant(r) }
            }

            impl TryFrom<ToolResult> for $result {
                type Error = ProtocolError;
                fn try_from(r: ToolResult) -> Result<Self, ProtocolError> {
                    match r {
                        ToolResult::$variant(v) => Ok(v),
                        other => Err(ProtocolError::Internal(format!(
                            "expected `{}` result, got `{}`", $name, other.name()
                        ))),
                    }
                }
            }
        )+

        /// LLM / MCP tool definitions for every tool in the catalog.
        #[must_use]
        pub fn tool_definitions() -> Vec<ToolDefinition> {
            vec![$(
                ToolDefinition {
                    name: $name,
                    description: $desc,
                    input_schema: schema::input_schema::<$params>(),
                    output_schema: schema::output_schema::<$result>(),
                },
            )+]
        }
    };
}

define_tools! {
    Bash => "bash", BashParams, BashResult,
        "Run a shell command with `bash -c` and wait for it to finish. Returns the exit code, stdout and stderr; long output is truncated keeping the head and tail. On timeout the command and all its children are killed. Each call starts a fresh shell, so `cd` and `export` do not persist: pass `cwd` and `env` instead. For servers, watchers or anything long-running, use `process_start`.";
    FileRead => "file_read", FileReadParams, FileReadResult,
        "Read a text file. Returns lines prefixed with their 1-based line numbers. Use `offset` and `limit` to page through large files. Binary files are detected and not returned; use `download` for those.";
    FileWrite => "file_write", FileWriteParams, FileWriteResult,
        "Create or overwrite a text file with the given content, creating parent directories as needed. Set `append` to add to the end instead. Prefer `file_edit` for changing part of an existing file.";
    FileEdit => "file_edit", FileEditParams, FileEditResult,
        "Replace an exact string in a file. `old_string` must match the file exactly, including whitespace and indentation, and must be unique unless `replace_all` is set; otherwise the edit fails and the file is unchanged. Read the file first. Returns a unified diff of the change.";
    FileDelete => "file_delete", FileDeleteParams, FileDeleteResult,
        "Delete a file, symlink or directory. Deleting a directory requires `recursive: true`.";
    FileMove => "file_move", FileMoveParams, FileMoveResult,
        "Move or rename a file or directory. Fails if the destination exists unless `overwrite` is set.";
    ListDir => "list_dir", ListDirParams, ListDirResult,
        "List a directory's entries with their kind, size and permission bits. Increase `depth` to recurse. Skips .gitignore'd entries by default.";
    Glob => "glob", GlobParams, GlobResult,
        "Find files whose paths match a glob pattern such as `**/*.rs` or `src/**/test_*.py`. Returns paths relative to `root`. Skips .gitignore'd files by default.";
    Grep => "grep", GrepParams, GrepResult,
        "Search file contents for a regular expression (or a literal string with `literal: true`). Returns each matching line with its file path, line number and optional context lines. Skips .gitignore'd files; narrow the search with `root` and `glob`.";
    ProcessStart => "process_start", ProcessStartParams, ProcessStartResult,
        "Start a long-running command in the background (dev server, file watcher, REPL) and return immediately with a `process_id`. Read its output with `process_poll`, send input with `process_stdin`, stop it with `process_kill`.";
    ProcessPoll => "process_poll", ProcessPollParams, ProcessPollResult,
        "Get a background process's status and the stdout/stderr it produced since the previous poll.";
    ProcessStdin => "process_stdin", ProcessStdinParams, ProcessStdinResult,
        "Write text to a background process's stdin. End with a newline to submit a line.";
    ProcessKill => "process_kill", ProcessKillParams, ProcessKillResult,
        "Stop a background process and all of its children.";
    ProcessList => "process_list", ProcessListParams, ProcessListResult,
        "List the background processes started with `process_start`, both running and exited.";
    Upload => "upload", UploadParams, UploadResult,
        "Write a file from base64-encoded bytes, e.g. an image or archive. Use `file_write` for text.";
    Download => "download", DownloadParams, DownloadResult,
        "Read a file as base64-encoded bytes, e.g. an image or archive. Use `file_read` for text.";
}
