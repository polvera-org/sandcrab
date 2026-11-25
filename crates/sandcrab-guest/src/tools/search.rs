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
//!
//! Implementation decisions (shared by all three tools, see [`walker`]):
//! - Hidden (dot) files and directories **are** listed / searched; only `.git` (dir or
//!   worktree file) is skipped. Agents routinely need `.github/`, `.env.example`, etc.
//! - `respect_gitignore` covers `.gitignore` (nested ones too, and in parent dirs of the
//!   root), `.ignore` and `.git/info/exclude`, whether or not the root is inside a git repo.
//!   The user's global git excludes file is never read, so results do not depend on
//!   `$HOME` config.
//! - `list_dir` siblings are ordered directories first, then by name (byte order); children
//!   follow their directory (depth-first). `depth` is capped at [`MAX_LIST_DEPTH`].
//! - `glob` returns paths sorted by path (byte order) — deterministic and diff-friendly.
//!   Symlinks pointing at files count as files. Patterns use `literal_separator`: `*`
//!   never crosses `/`, so `*.rs` matches top-level files only and `**/*.rs` is recursive.
//! - `grep`'s `glob` filter is matched against the file name when it has no `/` (so `*.ts`
//!   filters at any depth, like ripgrep `-g`), else against the root-relative path. It is not
//!   applied when the root is a single file. Per-file cap [`GREP_MAX_MATCHES_PER_FILE`],
//!   lines cut at [`GREP_MAX_LINE_CHARS`] chars, `context_lines` capped at
//!   [`GREP_MAX_CONTEXT_LINES`]; each match carries its own full context (overlapping context
//!   is repeated, never shared). Unreadable files are skipped silently.

mod glob_impl;
mod grep_impl;
mod list_impl;

use std::fs;
use std::io;
use std::path::Path;

use ignore::WalkBuilder;
use sandcrab_protocol::{
    GlobParams, GlobResult, GrepParams, GrepResult, ListDirParams, ListDirResult, ProtocolError,
};

use crate::ToolContext;

/// Deepest `list_dir` `depth` honoured; larger values are clamped.
pub const MAX_LIST_DEPTH: u64 = 32;
/// Max `grep` matches returned from a single file (hitting it sets `truncated`).
pub const GREP_MAX_MATCHES_PER_FILE: usize = 100;
/// `grep` match / context lines longer than this many chars are cut.
pub const GREP_MAX_LINE_CHARS: usize = 500;
/// Largest `grep` `context_lines` honoured; larger values are clamped.
pub const GREP_MAX_CONTEXT_LINES: u64 = 20;

/// List directory entries.
pub async fn list_dir(ctx: &ToolContext, p: ListDirParams) -> Result<ListDirResult, ProtocolError> {
    let root = ctx.resolve_path_or_default(p.path.as_deref());
    let depth = match p.depth {
        None => 1,
        Some(0) => {
            return Err(ProtocolError::InvalidParams(
                "`depth` must be at least 1 (1 lists direct children only)".into(),
            ));
        }
        Some(d) => d.min(MAX_LIST_DEPTH),
    };
    let max = ctx.limits.list_dir_max_entries;
    blocking(move || list_impl::run(&root, depth as usize, p.respect_gitignore, max)).await
}

/// Find files by glob pattern.
pub async fn glob(ctx: &ToolContext, p: GlobParams) -> Result<GlobResult, ProtocolError> {
    let root = ctx.resolve_path_or_default(p.root.as_deref());
    let matcher = glob_impl::compile(&p.pattern)?;
    let max = ctx.limits.glob_max_results;
    blocking(move || glob_impl::run(&root, &matcher, p.respect_gitignore, max)).await
}

/// Search file contents.
pub async fn grep(ctx: &ToolContext, p: GrepParams) -> Result<GrepResult, ProtocolError> {
    let root = ctx.resolve_path_or_default(p.root.as_deref());
    let opts = grep_impl::Options::new(&p, ctx.limits.grep_max_matches)?;
    blocking(move || grep_impl::run(&root, &opts)).await
}

/// Run blocking filesystem work off the async runtime.
async fn blocking<T, F>(f: F) -> Result<T, ProtocolError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ProtocolError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ProtocolError::Internal(format!("search task failed: {e}")))?
}

/// `stat` the search root (following a symlinked root), mapping errors for the caller.
fn stat_root(root: &Path) -> Result<fs::Metadata, ProtocolError> {
    fs::metadata(root).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => ProtocolError::NotFound(format!(
            "{} does not exist; check the path or list its parent directory",
            root.display()
        )),
        _ => ProtocolError::ExecutionFailed(format!("cannot access {}: {e}", root.display())),
    })
}

/// Error unless `root` exists and is a directory.
fn require_dir(root: &Path, tool: &str) -> Result<(), ProtocolError> {
    if stat_root(root)?.is_dir() {
        Ok(())
    } else {
        Err(ProtocolError::ExecutionFailed(format!(
            "{} is not a directory; `{tool}` needs a directory root",
            root.display()
        )))
    }
}

/// Walker shared by all tools: no symlink following, hidden files included, `.git` skipped,
/// ignore files applied iff `respect_gitignore` (see the module docs).
fn walker(root: &Path, respect_gitignore: bool) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.follow_links(false)
        .hidden(false)
        .git_global(false)
        .require_git(false)
        .git_ignore(respect_gitignore)
        .git_exclude(respect_gitignore)
        .ignore(respect_gitignore)
        .parents(respect_gitignore)
        .filter_entry(|e| e.depth() == 0 || e.file_name() != ".git");
    b
}

/// `path` relative to `root`, `/`-separated (lossy for non-UTF-8 names).
fn rel_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    parts.join("/")
}
