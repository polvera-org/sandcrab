//! `glob` walk.

use std::fs;
use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use sandcrab_protocol::{GlobResult, ProtocolError};

use super::{rel_path, require_dir, walker};

/// Compile a root-relative glob (`*` does not cross `/`; a leading `./` is ignored).
pub(super) fn compile(pattern: &str) -> Result<GlobMatcher, ProtocolError> {
    let pat = pattern.strip_prefix("./").unwrap_or(pattern);
    if pat.is_empty() {
        return Err(ProtocolError::InvalidParams(
            "glob `pattern` is empty; use e.g. `**/*.rs`".into(),
        ));
    }
    GlobBuilder::new(pat)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| ProtocolError::InvalidParams(format!("invalid glob pattern `{pattern}`: {e}")))
}

pub(super) fn run(
    root: &Path,
    matcher: &GlobMatcher,
    respect_gitignore: bool,
    max: usize,
) -> Result<GlobResult, ProtocolError> {
    require_dir(root, "glob")?;
    let mut b = walker(root, respect_gitignore);
    b.sort_by_file_name(|a, b| a.cmp(b));

    let mut paths = Vec::new();
    let mut truncated = false;
    for item in b.build() {
        let Ok(e) = item else { continue };
        if e.depth() == 0 {
            continue;
        }
        let is_file = e.file_type().is_some_and(|t| {
            t.is_file() || (t.is_symlink() && fs::metadata(e.path()).is_ok_and(|m| m.is_file()))
        });
        if !is_file {
            continue;
        }
        let rel = rel_path(root, e.path());
        if !matcher.is_match(&rel) {
            continue;
        }
        if paths.len() >= max {
            truncated = true;
            break;
        }
        paths.push(rel);
    }
    paths.sort();
    Ok(GlobResult { paths, truncated })
}
