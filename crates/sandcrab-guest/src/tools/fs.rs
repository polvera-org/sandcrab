//! Text file tools: `file_read` / `file_write` / `file_edit` / `file_delete` / `file_move`.
//!
//! Contract (see the `File*Params` / `File*Result` types in `sandcrab-protocol` and
//! `docs/ROADMAP.md`). Paths go through `ctx.resolve_path`; results report absolute paths.
//! Native Rust only — never shell out. Run blocking I/O in `spawn_blocking`.
//! - `file_read`: 1-based `offset` (default 1), `limit` (default
//!   `ctx.limits.file_read_default_lines`). Each line formatted `{n:>6}\t{line}` like `cat -n`;
//!   lines over `ctx.limits.file_read_max_line_chars` are cut and set `truncated`.
//!   Binary detection (NUL byte in the first 8 KiB, or invalid UTF-8) → `is_binary = true`,
//!   empty `content`. Missing file → `NotFound`; directory → `ExecutionFailed` (suggest
//!   `list_dir`); file over `ctx.limits.max_file_bytes` → `TooLarge` unless paging can stream.
//! - `file_write`: create parent dirs; overwrite atomically (temp file in the same dir +
//!   `rename`, preserving the existing mode); `append` appends in place. `created` = did not
//!   exist before. `bytes_written` = bytes of this call.
//! - `file_edit`: exact-match replace. `old_string == new_string` or empty `old_string` →
//!   `InvalidParams`. Zero matches → `NotFound` (hint: re-read the file). >1 match without
//!   `replace_all` → `AmbiguousEdit { path, occurrences }`, file untouched. Write atomically.
//!   `diff` = compact unified diff (`similar` crate, ~3 context lines).
//! - `file_delete`: file / symlink (never follow it) → remove; directory requires
//!   `recursive` else `ExecutionFailed` with a hint. Missing → `NotFound`. Report `kind`.
//! - `file_move`: create destination parents; existing destination without `overwrite` →
//!   `ExecutionFailed`. Try `rename`; on `EXDEV` fall back to copy + delete.

use sandcrab_protocol::{
    FileDeleteParams, FileDeleteResult, FileEditParams, FileEditResult, FileMoveParams,
    FileMoveResult, FileReadParams, FileReadResult, FileWriteParams, FileWriteResult,
    ProtocolError,
};

use crate::ToolContext;

/// Read a text file with line numbers.
pub async fn file_read(
    ctx: &ToolContext,
    p: FileReadParams,
) -> Result<FileReadResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("file_read"))
}

/// Create, overwrite or append to a text file.
pub async fn file_write(
    ctx: &ToolContext,
    p: FileWriteParams,
) -> Result<FileWriteResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("file_write"))
}

/// Exact string replacement in a file.
pub async fn file_edit(
    ctx: &ToolContext,
    p: FileEditParams,
) -> Result<FileEditResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("file_edit"))
}

/// Delete a file, symlink or directory.
pub async fn file_delete(
    ctx: &ToolContext,
    p: FileDeleteParams,
) -> Result<FileDeleteResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("file_delete"))
}

/// Move or rename a file or directory.
pub async fn file_move(
    ctx: &ToolContext,
    p: FileMoveParams,
) -> Result<FileMoveResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("file_move"))
}
