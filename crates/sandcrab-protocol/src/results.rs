//! Typed tool results.
//!
//! Results do not deny unknown fields so an older host keeps working against a newer
//! `crabd` that adds fields. Optional values are serialized as explicit `null`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Result of `bash`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BashResult {
    /// Exit status, or `null` if the command was killed by a signal.
    pub exit_code: Option<i32>,
    /// Signal number that terminated the command, if any.
    pub signal: Option<i32>,
    /// Captured stdout (head and tail kept when truncated).
    pub stdout: String,
    /// Captured stderr (head and tail kept when truncated).
    pub stderr: String,
    /// True if stdout or stderr was truncated.
    pub truncated: bool,
    /// True if the command hit its timeout and was killed.
    pub timed_out: bool,
    /// Wall-clock run time in milliseconds.
    pub duration_ms: u64,
}

/// Result of `file_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileReadResult {
    /// Requested lines, each formatted as `{line_number:>6}\t{line}` (like `cat -n`). Empty for binary files.
    pub content: String,
    /// Total number of lines in the file.
    pub total_lines: u64,
    /// 1-based number of the first returned line.
    pub start_line: u64,
    /// 1-based number of the last returned line (`start_line - 1` when nothing was returned).
    pub end_line: u64,
    /// True if the file has more lines after `end_line` or long lines were cut.
    pub truncated: bool,
    /// True if the file looks binary; `content` is then empty.
    pub is_binary: bool,
}

/// Result of `file_write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileWriteResult {
    /// Absolute path written.
    pub path: String,
    /// Number of bytes written by this call.
    pub bytes_written: u64,
    /// True if the file did not exist before.
    pub created: bool,
}

/// Result of `file_edit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileEditResult {
    /// Number of occurrences replaced.
    pub replacements: u64,
    /// Compact unified diff of the change.
    pub diff: String,
}

/// Kind of filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// Result of `file_delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileDeleteResult {
    /// Absolute path deleted.
    pub path: String,
    /// Kind of entry that was deleted.
    pub kind: EntryKind,
}

/// Result of `file_move`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileMoveResult {
    /// Absolute source path.
    pub from: String,
    /// Absolute destination path.
    pub to: String,
}

/// One `list_dir` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DirEntry {
    /// Path relative to the listed directory.
    pub path: String,
    /// Entry kind.
    pub kind: EntryKind,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Unix permission bits, e.g. 420 for 0644.
    pub mode: u32,
}

/// Result of `list_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ListDirResult {
    /// Entries in depth-first, name-sorted order.
    pub entries: Vec<DirEntry>,
    /// True if the entry limit was hit.
    pub truncated: bool,
}

/// Result of `glob`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GlobResult {
    /// Matching paths relative to the search root.
    pub paths: Vec<String>,
    /// True if the result limit was hit.
    pub truncated: bool,
}

/// One `grep` match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GrepMatch {
    /// File path relative to the search root.
    pub path: String,
    /// 1-based line number of the match.
    pub line_number: u64,
    /// The matching line, without its trailing newline.
    pub line: String,
    /// Lines immediately before the match.
    #[serde(default)]
    pub context_before: Vec<String>,
    /// Lines immediately after the match.
    #[serde(default)]
    pub context_after: Vec<String>,
}

/// Result of `grep`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GrepResult {
    /// Matches grouped by file, in line order.
    pub matches: Vec<GrepMatch>,
    /// True if the match limit was hit.
    pub truncated: bool,
}

/// Result of `process_start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessStartResult {
    /// Handle for `process_poll`, `process_stdin` and `process_kill`.
    pub process_id: String,
    /// OS process ID inside the sandbox.
    pub pid: u32,
}

/// Result of `process_poll`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessPollResult {
    /// True while the process is still running.
    pub running: bool,
    /// Exit status once exited normally.
    pub exit_code: Option<i32>,
    /// Terminating signal number, if killed by a signal.
    pub signal: Option<i32>,
    /// Stdout produced since the previous poll (PTY output appears here).
    pub stdout: String,
    /// Stderr produced since the previous poll.
    pub stderr: String,
    /// True if output was dropped because the buffer overflowed between polls.
    pub truncated: bool,
}

/// Result of `process_stdin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessStdinResult {
    /// Number of bytes written.
    pub bytes_written: u64,
}

/// Result of `process_kill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessKillResult {
    /// False if the process had already exited.
    pub was_running: bool,
}

/// One managed process in `process_list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessInfo {
    /// Handle returned by `process_start`.
    pub process_id: String,
    /// OS process ID inside the sandbox.
    pub pid: u32,
    /// Command line as started.
    pub command: String,
    /// True while running.
    pub running: bool,
    /// Exit status once exited normally.
    pub exit_code: Option<i32>,
    /// True if attached to a pseudo-terminal.
    pub pty: bool,
}

/// Result of `process_list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessListResult {
    /// Managed processes, oldest first.
    pub processes: Vec<ProcessInfo>,
}

/// Result of `upload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UploadResult {
    /// Absolute path written.
    pub path: String,
    /// Number of decoded bytes written.
    pub bytes_written: u64,
    /// True if the file did not exist before.
    pub created: bool,
}

/// Result of `download`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DownloadResult {
    /// Absolute path read.
    pub path: String,
    /// File content, base64-encoded (standard alphabet, padded).
    pub content_base64: String,
    /// File size in bytes.
    pub size: u64,
    /// Unix permission bits.
    pub mode: u32,
}

/// Result of the `hello` handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelloResult {
    /// `crabd` build version.
    pub crabd_version: String,
    /// Protocol version spoken by `crabd` (see [`crate::PROTOCOL_VERSION`]).
    pub protocol_version: String,
    /// Tool methods this daemon implements.
    pub tools: Vec<String>,
    /// Default working directory for relative paths and commands.
    pub default_cwd: String,
    /// What this daemon can do beyond the tools (see [`crate::features`]). Empty from a
    /// `crabd` before 0.3.
    #[serde(default)]
    pub features: Vec<String>,
    /// Loopback ports this daemon forwards (`--forward-port`). Empty from a `crabd` before 0.3.
    #[serde(default)]
    pub forward_ports: Vec<u16>,
}

/// Result of `ping`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PingResult {}
