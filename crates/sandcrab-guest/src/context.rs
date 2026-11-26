//! [`ToolContext`]: state shared (via `Arc`) by every tool call on every connection.

use std::path::{Component, Path, PathBuf};

use sandcrab_protocol::limits;

use crate::{supervise::ProcessTable, tools::process::ProcessRegistry};

/// Size / count / time limits enforced by the tool modules.
///
/// Defaults keep every result comfortably inside one frame and an LLM context window.
/// Tool modules read these instead of hard-coding constants so hosts and tests can tune them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `bash` timeout when the call omits `timeout_ms`.
    pub bash_default_timeout_ms: u64,
    /// Upper bound accepted for a caller-supplied `timeout_ms`.
    pub bash_max_timeout_ms: u64,
    /// Max bytes kept per stream (stdout / stderr) for `bash`; head + tail are kept.
    pub bash_max_output_bytes: usize,
    /// Max buffered, not-yet-polled output bytes per stream per background process.
    pub process_max_buffer_bytes: usize,
    /// Max managed background processes (running + exited, not yet reaped from the list).
    pub process_max_count: usize,
    /// `file_read` line limit when the call omits `limit`.
    pub file_read_default_lines: u64,
    /// Lines longer than this many chars are cut by `file_read`.
    pub file_read_max_line_chars: usize,
    /// Max decoded bytes for `upload` / `download` (and `file_read` / `file_edit` inputs).
    pub max_file_bytes: u64,
    /// Max entries returned by `list_dir`.
    pub list_dir_max_entries: usize,
    /// Max paths returned by `glob`.
    pub glob_max_results: usize,
    /// Max matches returned by `grep`.
    pub grep_max_matches: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bash_default_timeout_ms: limits::DEFAULT_BASH_TIMEOUT_MS,
            bash_max_timeout_ms: 60 * 60 * 1000,
            bash_max_output_bytes: 32 * 1024,
            process_max_buffer_bytes: 1024 * 1024,
            process_max_count: 64,
            file_read_default_lines: limits::DEFAULT_FILE_READ_LIMIT,
            file_read_max_line_chars: 2_000,
            max_file_bytes: limits::MAX_TRANSFER_BYTES,
            list_dir_max_entries: 1_000,
            glob_max_results: 1_000,
            grep_max_matches: 500,
        }
    }
}

/// Shared per-daemon state handed to every tool function.
///
/// Path policy: there is **no jail** here. The sandbox boundary is the VM / container;
/// host-side workspace guards (traversal, symlinks) live in `sandcrab-core`. Tools may touch
/// any path the `crabd serve` uid can reach.
#[derive(Debug)]
pub struct ToolContext {
    /// Absolute default working directory for relative paths and commands.
    pub default_cwd: PathBuf,
    /// Tool limits.
    pub limits: Limits,
    /// Background processes started by `process_start` (owned by `tools::process`).
    pub processes: ProcessRegistry,
    /// `process.open` programs and their supervisors (owned by `supervise`).
    pub supervisors: ProcessTable,
}

impl ToolContext {
    /// Context with default [`Limits`]. `default_cwd` is lexically normalised; if relative it
    /// is made absolute against the daemon's current directory.
    #[must_use]
    pub fn new(default_cwd: impl Into<PathBuf>) -> Self {
        Self::with_limits(default_cwd, Limits::default())
    }

    #[must_use]
    pub fn with_limits(default_cwd: impl Into<PathBuf>, limits: Limits) -> Self {
        let cwd = default_cwd.into();
        let cwd = if cwd.is_absolute() {
            cwd
        } else {
            std::path::absolute(&cwd).unwrap_or(cwd)
        };
        Self {
            default_cwd: normalize_lexical(&cwd),
            limits,
            processes: ProcessRegistry::default(),
            supervisors: ProcessTable::default(),
        }
    }

    /// Resolve a tool-supplied path: relative → joined onto `default_cwd`, absolute → as is;
    /// then `.` / `..` are normalised lexically (no filesystem access, symlinks untouched).
    /// An empty string resolves to `default_cwd`.
    #[must_use]
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        normalize_lexical(&self.default_cwd.join(path))
    }

    /// [`Self::resolve_path`] for optional path params; `None` → `default_cwd`.
    #[must_use]
    pub fn resolve_path_or_default(&self, path: Option<&str>) -> PathBuf {
        path.map_or_else(|| self.default_cwd.clone(), |p| self.resolve_path(p))
    }
}

/// Lexically normalise a path: drop `.`, fold `..` into its parent (never above `/`),
/// collapse repeated separators. Does not touch the filesystem.
#[must_use]
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // `pop` is a no-op at `/`; for a relative path with nothing left to pop, keep `..`.
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(name) => out.push(name),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::new("/workspace")
    }

    #[test]
    fn relative_joins_default_cwd() {
        assert_eq!(
            ctx().resolve_path("src/main.rs"),
            Path::new("/workspace/src/main.rs")
        );
        assert_eq!(ctx().resolve_path(""), Path::new("/workspace"));
        assert_eq!(ctx().resolve_path("."), Path::new("/workspace"));
    }

    #[test]
    fn absolute_kept() {
        assert_eq!(ctx().resolve_path("/etc/hosts"), Path::new("/etc/hosts"));
    }

    #[test]
    fn dot_dot_normalised_not_jailed() {
        assert_eq!(
            ctx().resolve_path("a/./b/../c"),
            Path::new("/workspace/a/c")
        );
        assert_eq!(ctx().resolve_path("../tmp/x"), Path::new("/tmp/x"));
        assert_eq!(ctx().resolve_path("../../../.."), Path::new("/"));
        assert_eq!(ctx().resolve_path("//a//b/"), Path::new("/a/b"));
    }

    #[test]
    fn optional_defaults_to_cwd() {
        assert_eq!(ctx().resolve_path_or_default(None), Path::new("/workspace"));
        assert_eq!(
            ctx().resolve_path_or_default(Some("x")),
            Path::new("/workspace/x")
        );
    }

    #[test]
    fn default_cwd_normalised() {
        let c = ToolContext::new("/workspace/./sub/..");
        assert_eq!(c.default_cwd, Path::new("/workspace"));
    }

    #[test]
    fn normalize_relative() {
        assert_eq!(
            normalize_lexical(Path::new("../a/./b")),
            Path::new("../a/b")
        );
        assert_eq!(normalize_lexical(Path::new("a/..")), Path::new("."));
    }
}
