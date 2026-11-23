//! Search tools: `list_dir` / `glob` / `grep`.
//!
//! Contract (see `ListDirParams` / `GlobParams` / `GrepParams` and their results in
//! `sandcrab-protocol`, and `docs/ROADMAP.md`). Native Rust only (`ignore`, `globset`,
//! `grep-searcher` + `grep-regex`) — never shell out to `find` / `grep`. Run the walk in
//! `spawn_blocking`. Roots go through `ctx.resolve_path_or_default`; missing root →
//! `NotFound`. Returned paths are relative to the root, `/`-separated. Do not follow
//! symlinks during walks. `.git/` is always skipped.
//! - `list_dir`: `depth` default 1 (direct children only); entries depth-first, name-sorted,
//!   with kind / size (0 for dirs) / permission bits. `respect_gitignore` (default true)
//!   applies .gitignore / .ignore / git exclude rules (also outside a git repo). Cap at
//!   `ctx.limits.list_dir_max_entries` and set `truncated`. Root not a directory →
//!   `ExecutionFailed`.
//! - `glob`: `globset` pattern matched against root-relative paths (files only), sorted.
//!   Bad pattern → `InvalidParams` with the parser message. Cap at
//!   `ctx.limits.glob_max_results`.
//! - `grep`: regex (or `literal`, escaped) with optional `case_insensitive`, `glob` filter
//!   and `context_lines` (before + after). Root may be a single file. Skip binary files.
//!   Matches grouped by file (sorted paths), in line order. Bad regex → `InvalidParams`.
//!   Cap at `ctx.limits.grep_max_matches`; cut very long lines.

use sandcrab_protocol::{
    GlobParams, GlobResult, GrepParams, GrepResult, ListDirParams, ListDirResult, ProtocolError,
};

use crate::ToolContext;

/// List directory entries.
pub async fn list_dir(ctx: &ToolContext, p: ListDirParams) -> Result<ListDirResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("list_dir"))
}

/// Find files by glob pattern.
pub async fn glob(ctx: &ToolContext, p: GlobParams) -> Result<GlobResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("glob"))
}

/// Search file contents.
pub async fn grep(ctx: &ToolContext, p: GrepParams) -> Result<GrepResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("grep"))
}
