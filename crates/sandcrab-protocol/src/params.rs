//! Typed tool parameters.
//!
//! Field doc comments become the LLM-facing JSON Schema descriptions, so they are written
//! as concise instructions to the model. Every struct rejects unknown fields.
//! Relative paths always resolve against the sandbox's default working directory.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const fn default_true() -> bool {
    true
}

/// Parameters for `bash`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashParams {
    /// Shell command to run with `bash -c`. Pipes, redirects, `&&` and multi-line scripts are allowed.
    pub command: String,
    /// Working directory for the command. Defaults to the sandbox working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment variables for this command only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Kill the command and all its child processes after this many milliseconds. Defaults to 120000 (2 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Parameters for `file_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileReadParams {
    /// Path of the file to read.
    pub path: String,
    /// 1-based line number to start reading from. Defaults to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Maximum number of lines to return. Defaults to 2000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

/// Parameters for `file_write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileWriteParams {
    /// Path of the file to write. Missing parent directories are created.
    pub path: String,
    /// Full text content to write.
    pub content: String,
    /// Append `content` to the end of the file instead of replacing it.
    #[serde(default)]
    pub append: bool,
}

/// Parameters for `file_edit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileEditParams {
    /// Path of the file to edit.
    pub path: String,
    /// Exact text to replace, including whitespace and indentation. Must occur exactly once unless `replace_all` is set.
    pub old_string: String,
    /// Replacement text. Must differ from `old_string`.
    pub new_string: String,
    /// Replace every occurrence of `old_string` instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

/// Parameters for `file_delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileDeleteParams {
    /// Path of the file, symlink or directory to delete.
    pub path: String,
    /// Required to delete a directory and everything inside it.
    #[serde(default)]
    pub recursive: bool,
}

/// Parameters for `file_move`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileMoveParams {
    /// Existing path to move.
    pub from: String,
    /// Destination path. Missing parent directories are created.
    pub to: String,
    /// Replace the destination if it already exists.
    #[serde(default)]
    pub overwrite: bool,
}

/// Parameters for `list_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDirParams {
    /// Directory to list. Defaults to the sandbox working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How many directory levels to descend. 1 lists only direct children. Defaults to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u64>,
    /// Skip entries ignored by .gitignore and similar ignore files.
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
}

/// Parameters for `glob`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobParams {
    /// Glob pattern matched against paths relative to `root`, e.g. `**/*.rs` or `src/**/test_*.py`.
    pub pattern: String,
    /// Directory to search from. Defaults to the sandbox working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Skip files ignored by .gitignore and similar ignore files.
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
}

/// Parameters for `grep`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepParams {
    /// Regular expression (Rust regex syntax) to search for, or a plain string when `literal` is set.
    pub pattern: String,
    /// File or directory to search. Defaults to the sandbox working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Only search files whose path matches this glob, e.g. `*.ts` or `src/**/*.py`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Treat `pattern` as a literal string instead of a regular expression.
    #[serde(default)]
    pub literal: bool,
    /// Match case-insensitively.
    #[serde(default)]
    pub case_insensitive: bool,
    /// Number of lines of context to include before and after each match. Defaults to 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lines: Option<u64>,
}

/// Parameters for `process_start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessStartParams {
    /// Shell command to run in the background with `bash -c`.
    pub command: String,
    /// Working directory for the process. Defaults to the sandbox working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment variables for this process only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Run attached to a pseudo-terminal. Use for interactive programs (REPLs, prompts) that need a TTY.
    #[serde(default)]
    pub pty: bool,
}

/// Parameters for `process_poll`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessPollParams {
    /// ID returned by `process_start`.
    pub process_id: String,
}

/// Parameters for `process_stdin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessStdinParams {
    /// ID returned by `process_start`.
    pub process_id: String,
    /// Text to write to the process's stdin. End with `\n` to submit a line.
    pub data: String,
}

/// Signal used by `process_kill`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum KillSignal {
    // Variants are undocumented on purpose: doc comments would make schemars emit a
    // verbose `oneOf` instead of a plain string `enum`.
    #[default]
    Term,
    Int,
    Kill,
}

/// Parameters for `process_kill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessKillParams {
    /// ID returned by `process_start`.
    pub process_id: String,
    /// Signal sent to the whole process group: `TERM` (graceful, default), `INT` (like Ctrl-C) or `KILL` (immediate).
    #[serde(default)]
    pub signal: KillSignal,
}

/// Parameters for `process_list` (none).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessListParams {}

/// Parameters for `upload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadParams {
    /// Destination file path. Missing parent directories are created.
    pub path: String,
    /// File content, base64-encoded (standard alphabet, padded). At most 10 MiB once decoded.
    pub content_base64: String,
    /// Unix permission bits as a decimal number, e.g. 420 for 0644 or 493 for 0755. Defaults to 0644.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Replace the file if it already exists.
    #[serde(default)]
    pub overwrite: bool,
}

/// Parameters for `download`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DownloadParams {
    /// Path of the file to download.
    pub path: String,
    /// Fail instead of returning files larger than this many bytes. Defaults to and is capped at 10485760 (10 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// Parameters for the `hello` handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    /// Protocol version spoken by the host, for compatibility diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// Free-form host client identifier, e.g. `sandcrab-core/0.1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// Parameters for `ping` (none).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PingParams {}
